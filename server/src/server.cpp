#include "diskost/server.hpp"

#include "diskost/json.hpp"
#include "diskost/ws.hpp"

#include <algorithm>
#include <cctype>
#include <chrono>
#include <condition_variable>
#include <cstdio>
#include <ctime>
#include <functional>
#include <mutex>
#include <random>
#include <sstream>
#include <stdexcept>
#include <unordered_map>
#include <utility>

namespace diskost {
namespace {

std::mutex g_log_mu;

void log_line(const char* level, const std::string& msg) {
  using clock = std::chrono::system_clock;
  const auto now = clock::now();
  const std::time_t t = clock::to_time_t(now);
  std::tm tm{};
#ifdef _WIN32
  localtime_s(&tm, &t);
#else
  localtime_r(&t, &tm);
#endif
  std::lock_guard<std::mutex> lock(g_log_mu);
  std::fprintf(stderr, "%02d:%02d:%02d %-5s %s\n", tm.tm_hour, tm.tm_min, tm.tm_sec, level,
               msg.c_str());
}

std::string lower_copy(std::string s) {
  for (char& c : s) c = static_cast<char>(std::tolower(static_cast<unsigned char>(c)));
  return s;
}

std::string trim_copy(std::string s) {
  auto not_space = [](unsigned char c) { return !std::isspace(c); };
  s.erase(s.begin(), std::find_if(s.begin(), s.end(), not_space));
  s.erase(std::find_if(s.rbegin(), s.rend(), not_space).base(), s.end());
  return s;
}

std::string clean_text(std::string s, std::size_t max_len, bool keep_newlines) {
  s = trim_copy(std::move(s));
  std::string out;
  out.reserve(std::min(s.size(), max_len));
  for (unsigned char c : s) {
    if (out.size() >= max_len) break;
    if (c == '\n' || c == '\t') {
      if (keep_newlines) out.push_back(static_cast<char>(c));
      continue;
    }
    if (c < 0x20) continue;
    out.push_back(static_cast<char>(c));
  }
  return trim_copy(std::move(out));
}

std::int64_t unix_ms() {
  const auto now = std::chrono::system_clock::now().time_since_epoch();
  return std::chrono::duration_cast<std::chrono::milliseconds>(now).count();
}

struct HttpRequest {
  std::string method;
  std::string path;
  std::vector<std::pair<std::string, std::string>> headers;
};

bool parse_http(const std::string& header, HttpRequest& out) {
  const auto line_end = header.find("\r\n");
  const std::string first = line_end == std::string::npos ? header : header.substr(0, line_end);
  std::istringstream ls(first);
  ls >> out.method >> out.path;
  if (out.method.empty() || out.path.empty()) return false;
  const auto q = out.path.find('?');
  if (q != std::string::npos) out.path.resize(q);
  std::size_t pos = line_end == std::string::npos ? header.size() : line_end + 2;
  while (pos < header.size()) {
    const auto next = header.find("\r\n", pos);
    const std::string line = header.substr(pos, next == std::string::npos ? std::string::npos : next - pos);
    if (line.empty()) break;
    const auto colon = line.find(':');
    if (colon != std::string::npos) {
      out.headers.emplace_back(lower_copy(trim_copy(line.substr(0, colon))),
                               trim_copy(line.substr(colon + 1)));
    }
    if (next == std::string::npos) break;
    pos = next + 2;
  }
  return true;
}

const std::string* header_value(const HttpRequest& req, const std::string& name) {
  for (const auto& h : req.headers) {
    if (h.first == name) return &h.second;
  }
  return nullptr;
}

bool send_http(socket_t fd, int code, const std::string& reason, const std::string& type,
               const std::string& body) {
  std::string msg = "HTTP/1.1 " + std::to_string(code) + " " + reason +
                    "\r\nContent-Type: " + type +
                    "\r\nContent-Length: " + std::to_string(body.size()) +
                    "\r\nConnection: close\r\n\r\n" + body;
  return send_all(fd, msg.data(), msg.size());
}

class Session : public std::enable_shared_from_this<Session> {
 public:
  Session(socket_t fd, std::string id) : fd_(fd), id_(std::move(id)) {}

  const std::string& id() const { return id_; }

  bool send_text(const std::string& text) { return send_frame(WS_TEXT, text); }

  bool send_frame(std::uint8_t opcode, std::string_view payload) {
    const std::string frame = encode_frame(opcode, payload, false);
    std::lock_guard<std::mutex> lock(write_mu_);
    if (shutting_ || fd_ == kInvalidSocket) return false;
    return send_all(fd_, frame.data(), frame.size());
  }

  // Unblock the read thread. The read thread is the one that closes the fd.
  void shutdown_io() {
    std::lock_guard<std::mutex> lock(write_mu_);
    if (shutting_) return;
    shutting_ = true;
    if (fd_ != kInvalidSocket) shutdown_socket(fd_);
  }

  template <typename OnText, typename OnClose>
  void run(OnText on_text, OnClose on_close, std::size_t max_message) {
    std::string acc;
    while (acc.find("\r\n\r\n") == std::string::npos) {
      if (acc.size() > 16384) {
        finish(on_close);
        return;
      }
      char buf[4096];
      const int n = recv_some(fd_unlocked(), buf, static_cast<int>(sizeof(buf)), 10000);
      if (n <= 0) {
        finish(on_close);
        return;
      }
      acc.append(buf, buf + n);
    }
    const auto header_end = acc.find("\r\n\r\n");
    const std::string header = acc.substr(0, header_end);
    const std::string rest = acc.substr(header_end + 4);

    HttpRequest req;
    if (!parse_http(header, req)) {
      send_http(raw_fd(), 400, "Bad Request", "text/plain", "bad request\n");
      finish(on_close);
      return;
    }
    const std::string* upgrade = header_value(req, "upgrade");
    const std::string* key = header_value(req, "sec-websocket-key");
    const bool websocket = upgrade && lower_copy(*upgrade) == "websocket" && key && !key->empty();
    if (!websocket) {
      if (req.method == "GET" && (req.path == "/health" || req.path == "/health/")) {
        send_http(raw_fd(), 200, "OK", "text/plain; charset=utf-8", "ok\n");
      } else if (req.method == "GET" && (req.path == "/" || req.path == "/ws")) {
        send_http(raw_fd(), 200, "OK", "text/plain; charset=utf-8",
                  "diskost meeting server\nWebSocket endpoint: /ws\nHealth: /health\n");
      } else {
        send_http(raw_fd(), 404, "Not Found", "text/plain; charset=utf-8", "not found\n");
      }
      finish(on_close);
      return;
    }

    const std::string response =
        "HTTP/1.1 101 Switching Protocols\r\n"
        "Upgrade: websocket\r\n"
        "Connection: Upgrade\r\n"
        "Sec-WebSocket-Accept: " +
        websocket_accept(*key) + "\r\n\r\n";
    {
      std::lock_guard<std::mutex> lock(write_mu_);
      if (shutting_ || fd_ == kInvalidSocket || !send_all(fd_, response.data(), response.size())) {
        if (fd_ != kInvalidSocket) {
          close_socket(fd_);
          fd_ = kInvalidSocket;
        }
        shutting_ = true;
        on_close(shared_from_this());
        return;
      }
    }

    WsDecoder decoder;
    decoder.set_require_mask(true);
    if (!rest.empty()) {
      decoder.append(reinterpret_cast<const std::uint8_t*>(rest.data()), rest.size());
    }
    int idle_rounds = 0;
    while (is_open()) {
      WsFrame frame;
      while (decoder.next(frame)) {
        idle_rounds = 0;
        if (!dispatch(frame, on_text, max_message)) {
          finish(on_close);
          return;
        }
      }
      if (decoder.failed()) {
        send_frame(WS_CLOSE, "");
        finish(on_close);
        return;
      }
      char buf[8192];
      const int n = recv_some(fd_unlocked(), buf, static_cast<int>(sizeof(buf)), 20000);
      if (!is_open()) break;
      if (n == 0) {
        if (++idle_rounds >= 3) break;
        send_frame(WS_PING, "diskost");
        continue;
      }
      if (n < 0) break;
      idle_rounds = 0;
      decoder.append(reinterpret_cast<const std::uint8_t*>(buf), static_cast<std::size_t>(n));
    }
    finish(on_close);
  }

 private:
  socket_t fd_unlocked() {
    std::lock_guard<std::mutex> lock(write_mu_);
    return fd_;
  }

  socket_t raw_fd() { return fd_unlocked(); }

  bool is_open() {
    std::lock_guard<std::mutex> lock(write_mu_);
    return !shutting_ && fd_ != kInvalidSocket;
  }

  template <typename OnClose>
  void finish(OnClose& on_close) {
    {
      std::lock_guard<std::mutex> lock(write_mu_);
      shutting_ = true;
      if (fd_ != kInvalidSocket) {
        close_socket(fd_);
        fd_ = kInvalidSocket;
      }
    }
    on_close(shared_from_this());
  }

  template <typename OnText>
  bool dispatch(const WsFrame& frame, OnText& on_text, std::size_t max_message) {
    switch (frame.opcode) {
      case WS_TEXT:
        if (frame.payload.size() > max_message) {
          send_frame(WS_CLOSE, "");
          return false;
        }
        try {
          on_text(shared_from_this(), frame.payload);
        } catch (const std::exception& ex) {
          log_line("error", std::string("handler: ") + ex.what());
        }
        return true;
      case WS_BINARY:
        send_frame(WS_CLOSE, "");
        return false;
      case WS_PING:
        send_frame(WS_PONG, frame.payload);
        return true;
      case WS_PONG:
        return true;
      case WS_CLOSE:
        send_frame(WS_CLOSE, "");
        return false;
      default:
        return true;
    }
  }

  socket_t fd_ = kInvalidSocket;
  std::string id_;
  bool shutting_ = false;
  std::mutex write_mu_;
};

struct Participant {
  std::string id;
  std::string session_id;
  std::string display_name;
  std::string room_id;
  bool audio = true;
  bool video = false;
  bool screen = false;
  std::shared_ptr<Session> session;
};

struct Room {
  std::string id;
  std::string name;
  std::string passcode;
  std::vector<std::string> member_ids;
};

struct Outgoing {
  std::shared_ptr<Session> session;
  std::string text;
};

Json participant_json(const Participant& p) {
  Json o = Json::object();
  o.set("id", Json::str(p.id));
  o.set("display_name", Json::str(p.display_name));
  o.set("audio", Json::boolean(p.audio));
  o.set("video", Json::boolean(p.video));
  o.set("screen", Json::boolean(p.screen));
  return o;
}

Json error_json(std::int64_t req, const std::string& code, const std::string& message) {
  Json o = Json::object();
  o.set("type", Json::str("error"));
  o.set("req", Json::number(req));
  o.set("code", Json::str(code));
  o.set("message", Json::str(message));
  return o;
}

void deliver(std::vector<Outgoing> msgs) {
  for (auto& msg : msgs) {
    if (msg.session) msg.session->send_text(msg.text);
  }
}

}  // namespace

struct MeetingServer::Impl {
  ServerConfig config;
  std::atomic<bool>* stop = nullptr;
  std::atomic<std::uint16_t>* bound_port = nullptr;
  TcpListener listener;
  std::mutex mu;
  std::condition_variable wait_cv;
  std::mutex wait_mu;
  std::atomic<int> active{0};
  std::unordered_map<std::string, std::shared_ptr<Session>> sessions;
  std::unordered_map<std::string, Participant> participants;
  std::unordered_map<std::string, std::string> session_to_participant;
  std::unordered_map<std::string, Room> rooms;
  std::mt19937_64 rng;

  explicit Impl(ServerConfig cfg) : config(std::move(cfg)) {
    std::random_device rd;
    std::seed_seq seq{rd(), rd(), rd(), rd()};
    rng.seed(seq);
  }

  std::string random_token(std::string_view alphabet, std::size_t n) {
    std::uniform_int_distribution<std::size_t> dist(0, alphabet.size() - 1);
    std::string out(n, '\0');
    for (std::size_t i = 0; i < n; ++i) out[i] = alphabet[dist(rng)];
    return out;
  }

  std::string make_room_id() {
    constexpr std::string_view kAlphabet = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    for (int i = 0; i < 8; ++i) {
      std::string id = random_token(kAlphabet, 8);
      if (!rooms.count(id)) return id;
    }
    return random_token(kAlphabet, 12);
  }

  std::string make_participant_id() {
    constexpr std::string_view kHex = "0123456789abcdef";
    for (int i = 0; i < 8; ++i) {
      std::string id = "p" + random_token(kHex, 10);
      if (!participants.count(id)) return id;
    }
    return "p" + random_token(kHex, 16);
  }

  std::string make_session_id() {
    constexpr std::string_view kHex = "0123456789abcdef";
    return "s" + random_token(kHex, 12);
  }

  void spawn(const std::shared_ptr<Session>& session) {
    active.fetch_add(1);
    std::thread([this, session] {
      session->run([this](const std::shared_ptr<Session>& s, const std::string& text) { on_text(s, text); },
                   [this](const std::shared_ptr<Session>& s) { on_close(s); },
                   config.max_message_bytes);
      if (active.fetch_sub(1) == 1) {
        std::lock_guard<std::mutex> lock(wait_mu);
        wait_cv.notify_all();
      }
    }).detach();
  }

  void on_text(const std::shared_ptr<Session>& session, const std::string& text) {
    Json msg;
    try {
      msg = Json::parse(text);
    } catch (const std::exception&) {
      deliver({{session, error_json(0, "invalid_json", "message is not valid json").dump()}});
      return;
    }
    if (!msg.is_object()) {
      deliver({{session, error_json(0, "invalid_json", "message must be an object").dump()}});
      return;
    }
    const std::string type = msg.get_string("type");
    const std::int64_t req = msg.get_int("req", 0);
    std::vector<Outgoing> out;
    {
      std::lock_guard<std::mutex> lock(mu);
      if (type == "create_room") out = handle_create(session, msg, req);
      else if (type == "join_room") out = handle_join(session, msg, req);
      else if (type == "leave") out = handle_leave(session, req);
      else if (type == "chat") out = handle_chat(session, msg, req);
      else if (type == "signal") out = handle_signal(session, msg, req);
      else if (type == "media_state") out = handle_media(session, msg, req);
      else if (type == "ping") out = handle_ping(session, msg, req);
      else out = {{session, error_json(req, "unknown_type", type.empty() ? "missing type" : type).dump()}};
    }
    deliver(std::move(out));
  }

  void on_close(const std::shared_ptr<Session>& session) {
    std::vector<Outgoing> out;
    {
      std::lock_guard<std::mutex> lock(mu);
      sessions.erase(session->id());
      out = take_leave(session->id(), false, 0);
    }
    deliver(std::move(out));
  }

  Participant* self_locked(const std::shared_ptr<Session>& session) {
    const auto it = session_to_participant.find(session->id());
    if (it == session_to_participant.end()) return nullptr;
    const auto pit = participants.find(it->second);
    if (pit == participants.end()) return nullptr;
    return &pit->second;
  }

  std::vector<Outgoing> fail(const std::shared_ptr<Session>& session, std::int64_t req,
                             const std::string& code, const std::string& message) {
    if (config.verbose) log_line("warn", code + ": " + message);
    return {{session, error_json(req, code, message).dump()}};
  }

  Json joined_message(const Room& room, const Participant& self, bool created, std::int64_t req) {
    Json parts = Json::array();
    for (const auto& id : room.member_ids) {
      const auto it = participants.find(id);
      if (it != participants.end()) parts.push(participant_json(it->second));
    }
    Json ice = Json::array();
    for (const auto& url : config.ice_servers) ice.push(Json::str(url));
    Json o = Json::object();
    o.set("type", Json::str("joined"));
    o.set("req", Json::number(req));
    o.set("room_id", Json::str(room.id));
    o.set("room_name", Json::str(room.name));
    o.set("self_id", Json::str(self.id));
    o.set("created", Json::boolean(created));
    o.set("participants", std::move(parts));
    o.set("ice_servers", std::move(ice));
    return o;
  }

  std::vector<Outgoing> handle_create(const std::shared_ptr<Session>& session, const Json& msg,
                                      std::int64_t req) {
    if (self_locked(session)) return fail(session, req, "already_in_room", "leave the current room first");
    if (static_cast<int>(rooms.size()) >= config.max_rooms) {
      return fail(session, req, "room_limit", "the server is not accepting new rooms");
    }
    std::string room_name = clean_text(msg.get_string("name"), 80, false);
    if (room_name.empty()) room_name = "Meeting";
    const std::string display = clean_text(msg.get_string("display_name"), 40, false);
    if (display.empty()) return fail(session, req, "name_required", "display_name is required");
    const std::string passcode = clean_text(msg.get_string("passcode"), 64, false);

    Room room;
    room.id = make_room_id();
    room.name = room_name;
    room.passcode = passcode;
    Participant person = make_person(session, display, room.id);
    room.member_ids.push_back(person.id);
    const std::string room_id = room.id;
    const std::string self_id = person.id;
    session_to_participant[person.session_id] = self_id;
    participants.emplace(self_id, std::move(person));
    rooms.emplace(room_id, std::move(room));
    const Json joined = joined_message(rooms.at(room_id), participants.at(self_id), true, req);
    log_line("info", "create " + room_id + " by " + display);
    return {{session, joined.dump()}};
  }

  std::vector<Outgoing> handle_join(const std::shared_ptr<Session>& session, const Json& msg,
                                    std::int64_t req) {
    if (self_locked(session)) return fail(session, req, "already_in_room", "leave the current room first");
    std::string room_id = clean_text(msg.get_string("room_id"), 16, false);
    for (char& c : room_id) c = static_cast<char>(std::toupper(static_cast<unsigned char>(c)));
    const auto rit = rooms.find(room_id);
    if (rit == rooms.end()) return fail(session, req, "room_not_found", "no room with that id");
    Room& room = rit->second;
    const std::string passcode = clean_text(msg.get_string("passcode"), 64, false);
    if (room.passcode != passcode) return fail(session, req, "bad_passcode", "wrong passcode");
    if (static_cast<int>(room.member_ids.size()) >= config.max_participants) {
      return fail(session, req, "room_full", "this room is full");
    }
    const std::string display = clean_text(msg.get_string("display_name"), 40, false);
    if (display.empty()) return fail(session, req, "name_required", "display_name is required");

    Participant person = make_person(session, display, room.id);
    const std::string self_id = person.id;
    room.member_ids.push_back(self_id);
    session_to_participant[person.session_id] = self_id;
    participants.emplace(self_id, std::move(person));
    const Json joined = joined_message(room, participants.at(self_id), false, req);
    Json arrived = Json::object();
    arrived.set("type", Json::str("participant_joined"));
    arrived.set("participant", participant_json(participants.at(self_id)));
    const std::string arrived_text = arrived.dump();
    std::vector<Outgoing> out{{session, joined.dump()}};
    for (const auto& id : room.member_ids) {
      if (id == self_id) continue;
      const auto it = participants.find(id);
      if (it != participants.end() && it->second.session) {
        out.push_back({it->second.session, arrived_text});
      }
    }
    log_line("info", "join " + room.id + " by " + display);
    return out;
  }

  Participant make_person(const std::shared_ptr<Session>& session, const std::string& display,
                          const std::string& room_id) {
    Participant person;
    person.id = make_participant_id();
    person.session_id = session->id();
    person.display_name = display;
    person.room_id = room_id;
    person.session = session;
    return person;
  }

  std::vector<Outgoing> handle_leave(const std::shared_ptr<Session>& session, std::int64_t req) {
    if (!self_locked(session)) return fail(session, req, "not_in_room", "join a room first");
    auto out = take_leave(session->id(), true, req);
    return out;
  }

  std::vector<Outgoing> take_leave(const std::string& session_id, bool ack, std::int64_t req) {
    const auto map_it = session_to_participant.find(session_id);
    if (map_it == session_to_participant.end()) return {};
    const std::string participant_id = map_it->second;
    session_to_participant.erase(map_it);
    const auto pit = participants.find(participant_id);
    if (pit == participants.end()) return {};
    const Participant person = pit->second;
    participants.erase(pit);

    std::vector<Outgoing> out;
    const auto rit = rooms.find(person.room_id);
    if (rit != rooms.end()) {
      auto& members = rit->second.member_ids;
      members.erase(std::remove(members.begin(), members.end(), participant_id), members.end());
      Json left = Json::object();
      left.set("type", Json::str("participant_left"));
      left.set("participant_id", Json::str(participant_id));
      const std::string text = left.dump();
      for (const auto& id : members) {
        const auto it = participants.find(id);
        if (it != participants.end() && it->second.session) out.push_back({it->second.session, text});
      }
      log_line("info", "leave " + person.room_id + " " + person.display_name);
      if (members.empty()) rooms.erase(rit);
    }
    if (ack && person.session) {
      Json done = Json::object();
      done.set("type", Json::str("left"));
      done.set("req", Json::number(req));
      out.insert(out.begin(), {person.session, done.dump()});
    }
    return out;
  }

  std::vector<Outgoing> handle_chat(const std::shared_ptr<Session>& session, const Json& msg,
                                    std::int64_t req) {
    Participant* self = self_locked(session);
    if (!self) return fail(session, req, "not_in_room", "join a room first");
    const std::string text = clean_text(msg.get_string("text"), 2000, true);
    if (text.empty()) return fail(session, req, "empty_chat", "chat text is empty");
    const auto rit = rooms.find(self->room_id);
    if (rit == rooms.end()) return fail(session, req, "not_in_room", "join a room first");
    Json chat = Json::object();
    chat.set("type", Json::str("chat_message"));
    chat.set("from", Json::str(self->id));
    chat.set("display_name", Json::str(self->display_name));
    chat.set("text", Json::str(text));
    chat.set("unix_ms", Json::number(unix_ms()));
    return fanout(rit->second, chat.dump());
  }

  std::vector<Outgoing> handle_signal(const std::shared_ptr<Session>& session, const Json& msg,
                                      std::int64_t req) {
    Participant* self = self_locked(session);
    if (!self) return fail(session, req, "not_in_room", "join a room first");
    const Json* payload = msg.get("payload");
    if (!payload || payload->is_null()) return fail(session, req, "missing_field", "payload is required");
    if (payload->dump().size() > (1 << 18)) {
      return fail(session, req, "payload_too_large", "signal payload is over 256 KiB");
    }
    const std::string to = msg.get_string("to");
    if (to.empty()) return fail(session, req, "missing_field", "to is required");
    const auto rit = rooms.find(self->room_id);
    if (rit == rooms.end()) return fail(session, req, "not_in_room", "join a room first");

    Json relay = Json::object();
    relay.set("type", Json::str("signal"));
    relay.set("from", Json::str(self->id));
    relay.set("payload", *payload);
    const std::string text = relay.dump();

    std::vector<Outgoing> out;
    if (to == "*") {
      for (const auto& id : rit->second.member_ids) {
        if (id == self->id) continue;
        const auto it = participants.find(id);
        if (it != participants.end() && it->second.session) out.push_back({it->second.session, text});
      }
      return out;
    }
    const auto it = participants.find(to);
    if (it == participants.end() || it->second.room_id != self->room_id) {
      return fail(session, req, "peer_not_found", "that participant is not in the room");
    }
    out.push_back({it->second.session, text});
    if (config.verbose) log_line("debug", "signal " + self->id + " -> " + to);
    return out;
  }

  std::vector<Outgoing> handle_media(const std::shared_ptr<Session>& session, const Json& msg,
                                     std::int64_t req) {
    (void)req;
    Participant* self = self_locked(session);
    if (!self) return fail(session, req, "not_in_room", "join a room first");
    if (msg.get("audio")) self->audio = msg.get_bool("audio", self->audio);
    if (msg.get("video")) self->video = msg.get_bool("video", self->video);
    if (msg.get("screen")) self->screen = msg.get_bool("screen", self->screen);
    const auto rit = rooms.find(self->room_id);
    if (rit == rooms.end()) return fail(session, req, "not_in_room", "join a room first");
    Json state = Json::object();
    state.set("type", Json::str("media_state"));
    state.set("participant_id", Json::str(self->id));
    state.set("audio", Json::boolean(self->audio));
    state.set("video", Json::boolean(self->video));
    state.set("screen", Json::boolean(self->screen));
    return fanout(rit->second, state.dump());
  }

  std::vector<Outgoing> handle_ping(const std::shared_ptr<Session>& session, const Json& msg,
                                    std::int64_t req) {
    Json pong = Json::object();
    pong.set("type", Json::str("pong"));
    pong.set("req", Json::number(req));
    pong.set("nonce", Json::number(msg.get_int("nonce", 0)));
    return {{session, pong.dump()}};
  }

  std::vector<Outgoing> fanout(const Room& room, const std::string& text) {
    std::vector<Outgoing> out;
    for (const auto& id : room.member_ids) {
      const auto it = participants.find(id);
      if (it != participants.end() && it->second.session) out.push_back({it->second.session, text});
    }
    return out;
  }

  void close_all_sessions() {
    std::vector<std::shared_ptr<Session>> copy;
    {
      std::lock_guard<std::mutex> lock(mu);
      for (auto& entry : sessions) copy.push_back(entry.second);
    }
    for (auto& session : copy) session->shutdown_io();
  }
};

MeetingServer::MeetingServer(ServerConfig config)
    : config_(std::move(config)), impl_(std::make_unique<Impl>(config_)) {
  impl_->stop = &stop_;
  impl_->bound_port = &bound_port_;
}

MeetingServer::~MeetingServer() { request_stop(); }

void MeetingServer::request_stop() {
  stop_.store(true);
  if (impl_) {
    impl_->listener.close();
    impl_->close_all_sessions();
  }
}

void MeetingServer::serve(const std::string& host, std::uint16_t port) {
  stop_.store(false);
  impl_->listener.listen_on(host, port);
  bound_port_.store(impl_->listener.port());
  log_line("info", "listening on " + host + ":" + std::to_string(bound_port_.load()));
  while (!stop_.load()) {
    const socket_t fd = impl_->listener.accept_one();
    if (fd == kInvalidSocket) {
      if (stop_.load()) break;
      continue;
    }
    if (stop_.load()) {
      close_socket(fd);
      break;
    }
    std::shared_ptr<Session> session;
    {
      std::lock_guard<std::mutex> lock(impl_->mu);
      session = std::make_shared<Session>(fd, impl_->make_session_id());
      impl_->sessions.emplace(session->id(), session);
    }
    impl_->spawn(session);
  }
  impl_->close_all_sessions();
  std::unique_lock<std::mutex> lock(impl_->wait_mu);
  impl_->wait_cv.wait(lock, [&] { return impl_->active.load() == 0; });
  bound_port_.store(0);
  log_line("info", "stopped");
}

}  // namespace diskost
