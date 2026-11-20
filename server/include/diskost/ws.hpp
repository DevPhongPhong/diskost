#pragma once

#include <cstddef>
#include <cstdint>
#include <string>
#include <string_view>
#include <vector>

namespace diskost {

constexpr std::uint8_t WS_CONT = 0x0;
constexpr std::uint8_t WS_TEXT = 0x1;
constexpr std::uint8_t WS_BINARY = 0x2;
constexpr std::uint8_t WS_CLOSE = 0x8;
constexpr std::uint8_t WS_PING = 0x9;
constexpr std::uint8_t WS_PONG = 0xA;

std::string sha1_hex(std::string_view data);
std::string sha1_bytes(std::string_view data);
std::string base64_encode(const std::uint8_t* data, std::size_t len);
std::string websocket_accept(std::string_view client_key);

struct WsFrame {
  std::uint8_t opcode = WS_TEXT;
  std::string payload;
};

class WsDecoder {
 public:
  void append(const std::uint8_t* data, std::size_t n);
  void set_require_mask(bool value) { require_mask_ = value; }
  // False means "need more data" unless failed() is set.
  bool next(WsFrame& out);
  bool failed() const { return !error_.empty(); }
  const std::string& error() const { return error_; }

 private:
  void compact();
  void fail(std::string message);

  std::vector<std::uint8_t> buf_;
  std::size_t off_ = 0;
  std::string error_;
  bool require_mask_ = false;
  std::string frag_;
  std::uint8_t frag_op_ = 0;
  bool in_frag_ = false;
};

// When mask is true the frame is masked with a random key (client -> server).
std::string encode_frame(std::uint8_t opcode, std::string_view payload, bool mask);

}  // namespace diskost
