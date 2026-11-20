#include "diskost/net.hpp"

#include <cerrno>
#include <stdexcept>

#ifndef _WIN32
#include <arpa/inet.h>
#include <netdb.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <signal.h>
#include <unistd.h>
#else
#include <mstcpip.h>
#endif

namespace diskost {
namespace {

std::runtime_error net_error(const std::string& what) {
#ifdef _WIN32
  return std::runtime_error(what + " (wsa " + std::to_string(WSAGetLastError()) + ")");
#else
  return std::runtime_error(what + " (" + std::to_string(errno) + ")");
#endif
}

}  // namespace

NetGuard::NetGuard() {
#ifdef _WIN32
  WSADATA data{};
  if (WSAStartup(MAKEWORD(2, 2), &data) != 0) {
    throw std::runtime_error("WSAStartup failed");
  }
#else
  std::signal(SIGPIPE, SIG_IGN);
#endif
}

NetGuard::~NetGuard() {
#ifdef _WIN32
  WSACleanup();
#endif
}

TcpListener::~TcpListener() { close(); }

void TcpListener::listen_on(const std::string& host, std::uint16_t port) {
  close();
  const std::string port_text = std::to_string(port);
  addrinfo hints{};
  hints.ai_family = AF_INET;
  hints.ai_socktype = SOCK_STREAM;
  hints.ai_protocol = IPPROTO_TCP;
  hints.ai_flags = AI_PASSIVE;
  addrinfo* result = nullptr;
  const char* host_arg = host.empty() ? nullptr : host.c_str();
  if (getaddrinfo(host_arg, port_text.c_str(), &hints, &result) != 0 || result == nullptr) {
    throw net_error("getaddrinfo failed for " + host);
  }
  socket_t fd = kInvalidSocket;
  int rc = 0;
  for (addrinfo* it = result; it != nullptr; it = it->ai_next) {
    fd = ::socket(it->ai_family, it->ai_socktype, it->ai_protocol);
    if (fd == kInvalidSocket) continue;
    int yes = 1;
    setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, reinterpret_cast<char*>(&yes), sizeof(yes));
    rc = ::bind(fd, it->ai_addr, static_cast<int>(it->ai_addrlen));
    if (rc == 0) break;
    close_socket(fd);
    fd = kInvalidSocket;
  }
  freeaddrinfo(result);
  if (fd == kInvalidSocket) throw net_error("bind failed on " + host + ":" + port_text);
  if (::listen(fd, 128) != 0) {
    close_socket(fd);
    throw net_error("listen failed");
  }
  fd_ = fd;
}

std::uint16_t TcpListener::port() const {
  if (fd_ == kInvalidSocket) return 0;
  sockaddr_in addr{};
  socklen_t len = sizeof(addr);
  if (getsockname(fd_, reinterpret_cast<sockaddr*>(&addr), &len) != 0) return 0;
  return ntohs(addr.sin_port);
}

socket_t TcpListener::accept_one() {
  if (fd_ == kInvalidSocket) return kInvalidSocket;
  sockaddr_in addr{};
  socklen_t len = sizeof(addr);
  socket_t fd = ::accept(fd_, reinterpret_cast<sockaddr*>(&addr), &len);
  if (fd == kInvalidSocket) return kInvalidSocket;
  int yes = 1;
  setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, reinterpret_cast<char*>(&yes), sizeof(yes));
  return fd;
}

void TcpListener::close() {
  if (fd_ != kInvalidSocket) {
    close_socket(fd_);
    fd_ = kInvalidSocket;
  }
}

void shutdown_socket(socket_t fd) {
  if (fd == kInvalidSocket) return;
#ifdef _WIN32
  ::shutdown(fd, SD_BOTH);
#else
  ::shutdown(fd, SHUT_RDWR);
#endif
}

void close_socket(socket_t fd) {
  if (fd == kInvalidSocket) return;
  shutdown_socket(fd);
#ifdef _WIN32
  ::closesocket(fd);
#else
  ::close(fd);
#endif
}

bool send_all(socket_t fd, const void* data, std::size_t len) {
  const auto* bytes = static_cast<const char*>(data);
  std::size_t off = 0;
  while (off < len) {
    const int chunk = static_cast<int>(len - off > 1 << 20 ? 1 << 20 : len - off);
#ifdef MSG_NOSIGNAL
    const int n = ::send(fd, bytes + off, chunk, MSG_NOSIGNAL);
#else
    const int n = ::send(fd, bytes + off, chunk, 0);
#endif
    if (n <= 0) return false;
    off += static_cast<std::size_t>(n);
  }
  return true;
}

int recv_some(socket_t fd, void* buf, int cap, int timeout_ms) {
  fd_set read_set;
  FD_ZERO(&read_set);
  FD_SET(fd, &read_set);
  timeval tv{};
  tv.tv_sec = timeout_ms / 1000;
  tv.tv_usec = (timeout_ms % 1000) * 1000;
#ifdef _WIN32
  const int rc = ::select(0, &read_set, nullptr, nullptr, &tv);
#else
  const int rc = ::select(static_cast<int>(fd) + 1, &read_set, nullptr, nullptr, &tv);
#endif
  if (rc == 0) return 0;
  if (rc < 0) return -1;
  const int n = ::recv(fd, static_cast<char*>(buf), cap, 0);
  if (n <= 0) return -1;
  return n;
}

}  // namespace diskost
