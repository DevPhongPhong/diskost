#pragma once

#include "diskost/net.hpp"

#include <atomic>
#include <cstddef>
#include <cstdint>
#include <deque>
#include <mutex>
#include <string>
#include <thread>
#include <unordered_map>
#include <vector>

namespace diskost {

// Receives recording datagrams. Packets with the DKST magic are kept in a
// small ring per room and participant. The payload stays encrypted.
class UdpRecorder {
 public:
  UdpRecorder() = default;
  ~UdpRecorder();

  UdpRecorder(const UdpRecorder&) = delete;
  UdpRecorder& operator=(const UdpRecorder&) = delete;

  void start(const std::string& host, std::uint16_t port, std::atomic<bool>* stop);
  void request_stop();
  void join();
  std::uint16_t port() const { return port_; }
  std::size_t count(const std::string& room, const std::string& participant) const;

 private:
  void loop();
  void store(const std::uint8_t* data, std::size_t len);

  socket_t fd_ = kInvalidSocket;
  std::uint16_t port_ = 0;
  std::atomic<bool>* stop_ = nullptr;
  std::thread thread_;
  mutable std::mutex mu_;
  std::unordered_map<std::string, std::deque<std::vector<std::uint8_t>>> rings_;
};

}  // namespace diskost
