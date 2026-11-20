#pragma once

#include <cstddef>
#include <cstdint>
#include <string>

#ifdef _WIN32
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <winsock2.h>
#include <ws2tcpip.h>
#else
#include <sys/socket.h>
#endif

namespace diskost {

#ifdef _WIN32
using socket_t = SOCKET;
inline constexpr socket_t kInvalidSocket = INVALID_SOCKET;
#else
using socket_t = int;
inline constexpr socket_t kInvalidSocket = -1;
#endif

class NetGuard {
 public:
  NetGuard();
  ~NetGuard();
  NetGuard(const NetGuard&) = delete;
  NetGuard& operator=(const NetGuard&) = delete;
};

class TcpListener {
 public:
  TcpListener() = default;
  ~TcpListener();
  TcpListener(const TcpListener&) = delete;
  TcpListener& operator=(const TcpListener&) = delete;

  void listen_on(const std::string& host, std::uint16_t port);
  std::uint16_t port() const;
  socket_t accept_one();
  void close();

 private:
  socket_t fd_ = kInvalidSocket;
};

void close_socket(socket_t fd);
void shutdown_socket(socket_t fd);
bool send_all(socket_t fd, const void* data, std::size_t len);
// >0 bytes, 0 timeout, <0 closed or error.
int recv_some(socket_t fd, void* buf, int cap, int timeout_ms);

}  // namespace diskost
