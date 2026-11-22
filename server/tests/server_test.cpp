#include "diskost/json.hpp"
#include "diskost/net.hpp"
#include "diskost/server.hpp"
#include "diskost/ws.hpp"

#ifndef _WIN32
#include <arpa/inet.h>
#include <netinet/in.h>
#endif

#include <chrono>
#include <cstdint>
#include <cstring>
#include <iostream>
#include <stdexcept>
#include <string>
#include <thread>

namespace {

int g_failures = 0;

void expect(bool cond, const std::string& message) {
  if (!cond) {
    std::cerr << "FAIL " << message << "\n";
    ++g_failures;
  }
}

[[noreturn]] void fail(const std::string& message) { throw std::runtime_error(message); }

class Client {
 public:
  explicit Client(std::uint16_t port) {
    fd_ = ::socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
    if (fd_ == diskost::kInvalidSocket) fail("socket");
    sockaddr_in addr{};
    addr.sin_family = AF_INET;
    addr.sin_port = htons(port);
    if (inet_pton(AF_INET, "127.0.0.1", &addr.sin_addr) != 1) fail("inet_pton");
    if (::connect(fd_, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) != 0) fail("connect");
    const std::string req =
        "GET /ws HTTP/1.1\r\n"
        "Host: 127.0.0.1\r\n"
        "Upgrade: websocket\r\n"
        "Connection: Upgrade\r\n"
        "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
        "Sec-WebSocket-Version: 13\r\n\r\n";
    if (!diskost::send_all(fd_, req.data(), req.size())) fail("handshake send");
    std::string acc;
    while (acc.find("\r\n\r\n") == std::string::npos) {
      char buf[1024];
      const int n = diskost::recv_some(fd_, buf, static_cast<int>(sizeof(buf)), 3000);
      if (n <= 0) fail("handshake timeout");
      acc.append(buf, buf + n);
    }
    if (acc.find("101") == std::string::npos) fail("missing 101: " + acc);
    if (acc.find("s3pPLMBiTxaQ9kYGzzhZRbK+xOo=") == std::string::npos) {
      fail("bad accept key: " + acc);
    }
    const auto header_end = acc.find("\r\n\r\n");
    const std::string rest = acc.substr(header_end + 4);
    if (!rest.empty()) {
      decoder_.append(reinterpret_cast<const std::uint8_t*>(rest.data()), rest.size());
    }
  }

  ~Client() { diskost::close_socket(fd_); }

  void send(const diskost::Json& message) {
    const std::string frame = diskost::encode_frame(diskost::WS_TEXT, message.dump(), true);
    if (!diskost::send_all(fd_, frame.data(), frame.size())) fail("send frame");
  }

  diskost::Json recv() {
    for (;;) {
      diskost::WsFrame frame;
      if (decoder_.next(frame)) {
        if (frame.opcode == diskost::WS_PING) {
          const std::string pong = diskost::encode_frame(diskost::WS_PONG, frame.payload, true);
          if (!diskost::send_all(fd_, pong.data(), pong.size())) fail("pong");
          continue;
        }
        if (frame.opcode == diskost::WS_TEXT) return diskost::Json::parse(frame.payload);
        continue;
      }
      if (decoder_.failed()) fail(decoder_.error());
      char buf[4096];
      const int n = diskost::recv_some(fd_, buf, static_cast<int>(sizeof(buf)), 3000);
      if (n <= 0) fail("timed out waiting for a message");
      decoder_.append(reinterpret_cast<const std::uint8_t*>(buf), static_cast<std::size_t>(n));
    }
  }

 private:
  diskost::socket_t fd_ = diskost::kInvalidSocket;
  diskost::WsDecoder decoder_;
};

void test_crypto_and_json() {
  expect(diskost::sha1_hex("") == "da39a3ee5e6b4b0d3255bfef95601890afd80709", "sha1 empty");
  expect(diskost::sha1_hex("abc") == "a9993e364706816aba3e25717850c26c9cd0d89d", "sha1 abc");
  expect(diskost::websocket_accept("dGhlIHNhbXBsZSBub25jZQ==") == "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=",
         "websocket accept");

  diskost::Json obj = diskost::Json::object();
  obj.set("type", diskost::Json::str("chat"));
  obj.set("text", diskost::Json::str("hi \"there\""));
  obj.set("req", diskost::Json::number(3));
  obj.set("n", diskost::Json::from_double(1.5));
  const diskost::Json parsed = diskost::Json::parse(obj.dump());
  expect(parsed.get_string("text") == "hi \"there\"", "json string roundtrip");
  expect(parsed.get_int("req") == 3, "json int roundtrip");
  expect(diskost::Json::parse("\"\\u0041\"").as_string() == "A", "json unicode");

  const std::string frame = diskost::encode_frame(diskost::WS_TEXT, "{\"a\":1}", true);
  diskost::WsDecoder decoder;
  decoder.set_require_mask(true);
  decoder.append(reinterpret_cast<const std::uint8_t*>(frame.data()), frame.size());
  diskost::WsFrame out;
  expect(decoder.next(out), "decode masked frame");
  expect(out.payload == "{\"a\":1}", "unmasked payload");
}

void test_meeting() {
  diskost::ServerConfig config;
  config.ice_servers = {"stun:stun.l.google.com:19302"};
  config.verbose = false;
  diskost::MeetingServer server(config);
  std::thread thread([&] {
    try {
      server.serve("127.0.0.1", 0);
    } catch (const std::exception& ex) {
      std::cerr << "server thread: " << ex.what() << "\n";
    }
  });
  for (int i = 0; i < 100 && server.bound_port() == 0; ++i) {
    std::this_thread::sleep_for(std::chrono::milliseconds(20));
  }
  const auto port = server.bound_port();
  expect(port != 0, "server bound a port");
  if (port == 0) {
    server.request_stop();
    thread.join();
    return;
  }

  try {
    Client alice(port);
    diskost::Json create = diskost::Json::object();
    create.set("type", diskost::Json::str("create_room"));
    create.set("req", diskost::Json::number(1));
    create.set("name", diskost::Json::str("Daily"));
    create.set("display_name", diskost::Json::str("Alice"));
    create.set("passcode", diskost::Json::str("secret"));
    alice.send(create);
    const diskost::Json created = alice.recv();
    expect(created.get_string("type") == "joined", "create replies joined");
    expect(created.get_bool("created"), "room marked created");
    expect(created.get_int("req") == 1, "create echoes req");
    const std::string room_id = created.get_string("room_id");
    const std::string alice_id = created.get_string("self_id");
    expect(room_id.size() >= 8, "room id assigned");
    expect(created.get("participants") && created.get("participants")->as_array().size() == 1,
           "creator is in the roster");
    expect(created.get("ice_servers") && created.get("ice_servers")->as_array().size() == 1,
           "stun list is advertised");

    Client bob(port);
    diskost::Json bad = diskost::Json::object();
    bad.set("type", diskost::Json::str("join_room"));
    bad.set("req", diskost::Json::number(2));
    bad.set("room_id", diskost::Json::str(room_id));
    bad.set("display_name", diskost::Json::str("Bob"));
    bad.set("passcode", diskost::Json::str("nope"));
    bob.send(bad);
    const diskost::Json denied = bob.recv();
    expect(denied.get_string("code") == "bad_passcode", "wrong passcode is rejected");

    diskost::Json join = bad;
    join.set("req", diskost::Json::number(3));
    join.set("passcode", diskost::Json::str("secret"));
    bob.send(join);
    const diskost::Json joined = bob.recv();
    expect(joined.get_string("type") == "joined", "bob joined");
    expect(joined.get("participants") && joined.get("participants")->as_array().size() == 2,
           "roster has two people");
    const diskost::Json arrived = alice.recv();
    expect(arrived.get_string("type") == "participant_joined", "alice sees bob");
    const std::string bob_id = joined.get_string("self_id");

    diskost::Json chat = diskost::Json::object();
    chat.set("type", diskost::Json::str("chat"));
    chat.set("req", diskost::Json::number(4));
    chat.set("text", diskost::Json::str("hello"));
    bob.send(chat);
    const diskost::Json bob_sees = bob.recv();
    const diskost::Json alice_sees = alice.recv();
    expect(bob_sees.get_string("type") == "chat_message", "sender sees chat");
    expect(alice_sees.get_string("text") == "hello", "alice receives chat");
    expect(alice_sees.get_string("display_name") == "Bob", "chat has display name");

    diskost::Json signal = diskost::Json::object();
    signal.set("type", diskost::Json::str("signal"));
    signal.set("req", diskost::Json::number(5));
    signal.set("to", diskost::Json::str(bob_id));
    diskost::Json payload = diskost::Json::object();
    payload.set("kind", diskost::Json::str("offer"));
    payload.set("sdp", diskost::Json::str("v=0"));
    signal.set("payload", std::move(payload));
    alice.send(signal);
    const diskost::Json relayed = bob.recv();
    expect(relayed.get_string("type") == "signal", "signal relayed");
    expect(relayed.get_string("from") == alice_id, "signal from alice");
    expect(relayed.get("payload") && relayed.get("payload")->get_string("kind") == "offer",
           "offer payload kept");

    diskost::Json media = diskost::Json::object();
    media.set("type", diskost::Json::str("media_state"));
    media.set("req", diskost::Json::number(6));
    media.set("audio", diskost::Json::boolean(false));
    media.set("video", diskost::Json::boolean(true));
    media.set("screen", diskost::Json::boolean(false));
    bob.send(media);
    const diskost::Json bob_media = bob.recv();
    const diskost::Json alice_media = alice.recv();
    expect(alice_media.get_string("participant_id") == bob_id, "media names bob");
    expect(alice_media.get_bool("video") && !alice_media.get_bool("audio"), "media flags");
    expect(bob_media.get_bool("video"), "bob sees his own media state");

    diskost::Json leave = diskost::Json::object();
    leave.set("type", diskost::Json::str("leave"));
    leave.set("req", diskost::Json::number(7));
    alice.send(leave);
    const diskost::Json left = alice.recv();
    const diskost::Json bob_notice = bob.recv();
    expect(left.get_string("type") == "left", "alice is told she left");
    expect(bob_notice.get_string("type") == "participant_left", "bob sees alice leave");
    expect(bob_notice.get_string("participant_id") == alice_id, "left id is alice");
  } catch (const std::exception& ex) {
    std::cerr << "FAIL meeting flow: " << ex.what() << "\n";
    ++g_failures;
  }

  server.request_stop();
  thread.join();
}

void test_udp_record() {
  diskost::ServerConfig config;
  config.udp_port = 0;
  diskost::MeetingServer server(std::move(config));
  std::thread thread([&server] { server.serve("127.0.0.1", 0); });
  for (int i = 0; i < 50 && server.bound_udp_port() == 0; ++i) {
    std::this_thread::sleep_for(std::chrono::milliseconds(20));
  }
  expect(server.bound_udp_port() != 0, "udp port is bound");

  const std::uint8_t good[] = {'D', 'K', 'S', 'T', 1, 1, 3, 7, 0, 0, 0, 0xe8, 3, 0, 0, 0, 0, 0, 0,
                                7, 0, 0, 0, 0, 0, 1, 0, 2, 'A', 'B', 2, 'p', '1', 1, 2, 3};
  std::uint8_t bad[sizeof(good)];
  std::memcpy(bad, good, sizeof(good));
  bad[0] = 'X';

  const diskost::socket_t fd = ::socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
  expect(fd != diskost::kInvalidSocket, "udp socket");
  sockaddr_in addr{};
  addr.sin_family = AF_INET;
  addr.sin_port = htons(server.bound_udp_port());
  expect(inet_pton(AF_INET, "127.0.0.1", &addr.sin_addr) == 1, "udp address");
  expect(::sendto(fd, reinterpret_cast<const char*>(bad), static_cast<int>(sizeof(bad)), 0,
                  reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) == static_cast<int>(sizeof(bad)),
         "send bad magic");
  expect(::sendto(fd, reinterpret_cast<const char*>(good), static_cast<int>(sizeof(good)), 0,
                  reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) == static_cast<int>(sizeof(good)),
         "send recording");
  diskost::close_socket(fd);

  for (int i = 0; i < 50 && server.recorded_count("AB", "p1") == 0; ++i) {
    std::this_thread::sleep_for(std::chrono::milliseconds(20));
  }
  expect(server.recorded_count("AB", "p1") == 1, "valid datagram stored once");
  expect(server.recorded_count("nope", "p1") == 0, "unknown room stays empty");

  server.request_stop();
  thread.join();
}

}  // namespace

int main() {
  try {
    test_crypto_and_json();
    test_meeting();
    test_udp_record();
  } catch (const std::exception& ex) {
    std::cerr << "FAIL " << ex.what() << "\n";
    return 1;
  }
  if (g_failures) {
    std::cerr << g_failures << " failure(s)\n";
    return 1;
  }
  std::cout << "ok\n";
  return 0;
}
