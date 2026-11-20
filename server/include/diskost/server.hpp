#pragma once

#include "diskost/net.hpp"

#include <atomic>
#include <cstdint>
#include <memory>
#include <string>
#include <thread>
#include <vector>

namespace diskost {

struct ServerConfig {
  int max_participants = 16;
  int max_rooms = 1000;
  std::size_t max_message_bytes = 1 << 20;
  bool verbose = false;
  std::vector<std::string> ice_servers{"stun:stun.l.google.com:19302"};
};

class MeetingServer {
 public:
  explicit MeetingServer(ServerConfig config = {});
  ~MeetingServer();

  MeetingServer(const MeetingServer&) = delete;
  MeetingServer& operator=(const MeetingServer&) = delete;

  // Binds, then blocks until request_stop(). Port 0 asks the OS for a port.
  void serve(const std::string& host, std::uint16_t port);
  void request_stop();
  std::uint16_t bound_port() const { return bound_port_.load(); }

 private:
  struct Impl;
  NetGuard net_;
  ServerConfig config_;
  std::unique_ptr<Impl> impl_;
  std::atomic<std::uint16_t> bound_port_{0};
  std::atomic<bool> stop_{false};
};

}  // namespace diskost
