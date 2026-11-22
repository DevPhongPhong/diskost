#include "diskost/record.hpp"

#include <cstring>
#include <stdexcept>
#include <string>

#ifndef _WIN32
#include <arpa/inet.h>
#include <netdb.h>
#include <netinet/in.h>
#include <unistd.h>
#else
#include <ws2tcpip.h>
#endif

namespace diskost {
namespace {

std::string ring_key(const std::string& room, const std::string& participant) {
  return room + "\n" + participant;
}

bool read_ids(const std::uint8_t* data, std::size_t len, std::string& room, std::string& participant) {
  if (len < 28 || std::memcmp(data, "DKST", 4) != 0 || data[4] != 1) return false;
  const std::size_t room_len = data[27];
  if (len < 28 + room_len + 1) return false;
  room.assign(reinterpret_cast<const char*>(data + 28), room_len);
  const std::size_t part_at = 28 + room_len;
  const std::size_t part_len = data[part_at];
  if (len < part_at + 1 + part_len) return false;
  participant.assign(reinterpret_cast<const char*>(data + part_at + 1), part_len);
  return true;
}

}  // namespace

UdpRecorder::~UdpRecorder() {
  request_stop();
  join();
}

void UdpRecorder::start(const std::string& host, std::uint16_t port, std::atomic<bool>* stop) {
  request_stop();
  join();
  stop_ = stop;
  const std::string port_text = std::to_string(port);
  addrinfo hints{};
  hints.ai_family = AF_INET;
  hints.ai_socktype = SOCK_DGRAM;
  hints.ai_protocol = IPPROTO_UDP;
  hints.ai_flags = AI_PASSIVE;
  addrinfo* result = nullptr;
  const char* host_arg = host.empty() ? nullptr : host.c_str();
  if (getaddrinfo(host_arg, port_text.c_str(), &hints, &result) != 0 || result == nullptr) {
    throw std::runtime_error("udp getaddrinfo failed for " + host);
  }
  socket_t fd = kInvalidSocket;
  for (addrinfo* it = result; it != nullptr; it = it->ai_next) {
    fd = ::socket(it->ai_family, it->ai_socktype, it->ai_protocol);
    if (fd == kInvalidSocket) continue;
    int yes = 1;
    setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, reinterpret_cast<char*>(&yes), sizeof(yes));
    if (::bind(fd, it->ai_addr, static_cast<int>(it->ai_addrlen)) == 0) break;
    close_socket(fd);
    fd = kInvalidSocket;
  }
  freeaddrinfo(result);
  if (fd == kInvalidSocket) throw std::runtime_error("udp bind failed on " + host + ":" + port_text);

  sockaddr_in addr{};
  socklen_t len = sizeof(addr);
  if (getsockname(fd, reinterpret_cast<sockaddr*>(&addr), &len) != 0) {
    close_socket(fd);
    throw std::runtime_error("udp getsockname failed");
  }
  {
    std::lock_guard<std::mutex> lock(mu_);
    fd_ = fd;
    port_ = ntohs(addr.sin_port);
  }
  thread_ = std::thread([this] { loop(); });
}

void UdpRecorder::request_stop() {
  socket_t fd = kInvalidSocket;
  {
    std::lock_guard<std::mutex> lock(mu_);
    fd = fd_;
    fd_ = kInvalidSocket;
  }
  if (fd != kInvalidSocket) close_socket(fd);
}

void UdpRecorder::join() {
  if (thread_.joinable()) thread_.join();
}

std::size_t UdpRecorder::count(const std::string& room, const std::string& participant) const {
  std::lock_guard<std::mutex> lock(mu_);
  const auto it = rings_.find(ring_key(room, participant));
  if (it == rings_.end()) return 0;
  return it->second.size();
}

void UdpRecorder::loop() {
  for (;;) {
    if (stop_ != nullptr && stop_->load()) break;
    socket_t fd = kInvalidSocket;
    {
      std::lock_guard<std::mutex> lock(mu_);
      fd = fd_;
    }
    if (fd == kInvalidSocket) break;

    fd_set read_set;
    FD_ZERO(&read_set);
    FD_SET(fd, &read_set);
    timeval tv{};
    tv.tv_sec = 0;
    tv.tv_usec = 200000;
#ifdef _WIN32
    const int ready = ::select(0, &read_set, nullptr, nullptr, &tv);
#else
    const int ready = ::select(static_cast<int>(fd) + 1, &read_set, nullptr, nullptr, &tv);
#endif
    if (ready < 0) break;
    if (ready == 0) continue;

    char buf[2048];
    sockaddr_in from{};
    socklen_t from_len = sizeof(from);
    const int n = ::recvfrom(fd, buf, static_cast<int>(sizeof(buf)), 0, reinterpret_cast<sockaddr*>(&from),
                             &from_len);
    if (n > 0) store(reinterpret_cast<const std::uint8_t*>(buf), static_cast<std::size_t>(n));
  }
}

void UdpRecorder::store(const std::uint8_t* data, std::size_t len) {
  std::string room;
  std::string participant;
  if (!read_ids(data, len, room, participant)) return;
  std::vector<std::uint8_t> copy(data, data + len);
  std::lock_guard<std::mutex> lock(mu_);
  auto& ring = rings_[ring_key(room, participant)];
  ring.push_back(std::move(copy));
  while (ring.size() > 32) ring.pop_front();
}

}  // namespace diskost
