#include "diskost/ws.hpp"

#include <array>
#include <cstring>

namespace diskost {
namespace {

constexpr std::uint32_t rol(std::uint32_t v, int n) { return (v << n) | (v >> (32 - n)); }

class Sha1 {
 public:
  Sha1() { reset(); }

  void update(const std::uint8_t* data, std::size_t len) {
    for (std::size_t i = 0; i < len; ++i) {
      block_[block_len_++] = data[i];
      if (block_len_ == 64) {
        transform(block_.data());
        bit_len_ += 512;
        block_len_ = 0;
      }
    }
  }

  void update(std::string_view text) {
    update(reinterpret_cast<const std::uint8_t*>(text.data()), text.size());
  }

  std::array<std::uint8_t, 20> final() {
    std::uint64_t total_bits = bit_len_ + static_cast<std::uint64_t>(block_len_) * 8;
    block_[block_len_++] = 0x80;
    if (block_len_ > 56) {
      while (block_len_ < 64) block_[block_len_++] = 0;
      transform(block_.data());
      block_len_ = 0;
    }
    while (block_len_ < 56) block_[block_len_++] = 0;
    for (int i = 7; i >= 0; --i) {
      block_[block_len_++] = static_cast<std::uint8_t>((total_bits >> (i * 8)) & 0xFF);
    }
    transform(block_.data());

    std::array<std::uint8_t, 20> out{};
    for (int i = 0; i < 5; ++i) {
      out[i * 4] = static_cast<std::uint8_t>((h_[i] >> 24) & 0xFF);
      out[i * 4 + 1] = static_cast<std::uint8_t>((h_[i] >> 16) & 0xFF);
      out[i * 4 + 2] = static_cast<std::uint8_t>((h_[i] >> 8) & 0xFF);
      out[i * 4 + 3] = static_cast<std::uint8_t>(h_[i] & 0xFF);
    }
    return out;
  }

 private:
  void reset() {
    h_ = {0x67452301u, 0xEFCDAB89u, 0x98BADCFEu, 0x10325476u, 0xC3D2E1F0u};
    block_len_ = 0;
    bit_len_ = 0;
  }

  void transform(const std::uint8_t* chunk) {
    std::uint32_t w[80];
    for (int i = 0; i < 16; ++i) {
      w[i] = (std::uint32_t(chunk[i * 4]) << 24) | (std::uint32_t(chunk[i * 4 + 1]) << 16) |
             (std::uint32_t(chunk[i * 4 + 2]) << 8) | std::uint32_t(chunk[i * 4 + 3]);
    }
    for (int i = 16; i < 80; ++i) {
      w[i] = rol(w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16], 1);
    }
    std::uint32_t a = h_[0], b = h_[1], c = h_[2], d = h_[3], e = h_[4];
    for (int i = 0; i < 80; ++i) {
      std::uint32_t f = 0;
      std::uint32_t k = 0;
      if (i < 20) {
        f = (b & c) | ((~b) & d);
        k = 0x5A827999u;
      } else if (i < 40) {
        f = b ^ c ^ d;
        k = 0x6ED9EBA1u;
      } else if (i < 60) {
        f = (b & c) | (b & d) | (c & d);
        k = 0x8F1BBCDCu;
      } else {
        f = b ^ c ^ d;
        k = 0xCA62C1D6u;
      }
      const std::uint32_t temp = rol(a, 5) + f + e + k + w[i];
      e = d;
      d = c;
      c = rol(b, 30);
      b = a;
      a = temp;
    }
    h_[0] += a;
    h_[1] += b;
    h_[2] += c;
    h_[3] += d;
    h_[4] += e;
  }

  std::array<std::uint32_t, 5> h_{};
  std::array<std::uint8_t, 64> block_{};
  std::size_t block_len_ = 0;
  std::uint64_t bit_len_ = 0;
};

}  // namespace

std::string sha1_bytes(std::string_view data) {
  Sha1 sha;
  sha.update(data);
  const auto dig = sha.final();
  return std::string(reinterpret_cast<const char*>(dig.data()), dig.size());
}

std::string sha1_hex(std::string_view data) {
  const std::string raw = sha1_bytes(data);
  static constexpr char kHex[] = "0123456789abcdef";
  std::string out;
  out.resize(raw.size() * 2);
  for (std::size_t i = 0; i < raw.size(); ++i) {
    const auto b = static_cast<unsigned char>(raw[i]);
    out[i * 2] = kHex[b >> 4];
    out[i * 2 + 1] = kHex[b & 0xF];
  }
  return out;
}

std::string base64_encode(const std::uint8_t* data, std::size_t len) {
  static constexpr char kTable[] =
      "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  std::string out;
  out.reserve(((len + 2) / 3) * 4);
  std::size_t i = 0;
  while (i + 3 <= len) {
    const std::uint32_t n = (std::uint32_t(data[i]) << 16) | (std::uint32_t(data[i + 1]) << 8) |
                            std::uint32_t(data[i + 2]);
    out.push_back(kTable[(n >> 18) & 63]);
    out.push_back(kTable[(n >> 12) & 63]);
    out.push_back(kTable[(n >> 6) & 63]);
    out.push_back(kTable[n & 63]);
    i += 3;
  }
  if (i < len) {
    std::uint32_t n = std::uint32_t(data[i]) << 16;
    out.push_back(kTable[(n >> 18) & 63]);
    if (i + 1 < len) {
      n |= std::uint32_t(data[i + 1]) << 8;
      out.push_back(kTable[(n >> 12) & 63]);
      out.push_back(kTable[(n >> 6) & 63]);
      out.push_back('=');
    } else {
      out.push_back(kTable[(n >> 12) & 63]);
      out.push_back('=');
      out.push_back('=');
    }
  }
  return out;
}

std::string websocket_accept(std::string_view client_key) {
  const std::string material =
      std::string(client_key) + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
  const std::string dig = sha1_bytes(material);
  return base64_encode(reinterpret_cast<const std::uint8_t*>(dig.data()), dig.size());
}

}  // namespace diskost
