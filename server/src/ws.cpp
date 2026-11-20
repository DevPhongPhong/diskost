#include "diskost/ws.hpp"

#include <cstring>
#include <random>

namespace diskost {
namespace {

constexpr std::size_t kMaxPayload = 1 << 20;

std::uint64_t read_be(const std::uint8_t* p, int nbytes) {
  std::uint64_t v = 0;
  for (int i = 0; i < nbytes; ++i) v = (v << 8) | p[i];
  return v;
}

}  // namespace

void WsDecoder::append(const std::uint8_t* data, std::size_t n) {
  buf_.insert(buf_.end(), data, data + n);
}

void WsDecoder::compact() {
  if (off_ == 0) return;
  if (off_ >= buf_.size()) {
    buf_.clear();
    off_ = 0;
    return;
  }
  if (off_ > 4096) {
    buf_.erase(buf_.begin(), buf_.begin() + static_cast<std::ptrdiff_t>(off_));
    off_ = 0;
  }
}

void WsDecoder::fail(std::string message) { error_ = std::move(message); }

bool WsDecoder::next(WsFrame& out) {
  if (failed()) return false;
  while (true) {
    const std::size_t avail = buf_.size() - off_;
    if (avail < 2) {
      compact();
      return false;
    }
    const std::uint8_t* p = buf_.data() + off_;
    const bool fin = (p[0] & 0x80) != 0;
    const std::uint8_t opcode = p[0] & 0x0F;
    const bool masked = (p[1] & 0x80) != 0;
    std::uint64_t len = p[1] & 0x7F;
    std::size_t header = 2;
    if (len == 126) {
      header += 2;
    } else if (len == 127) {
      header += 8;
    }
    if (masked) header += 4;
    if (avail < header) {
      compact();
      return false;
    }
    std::size_t cursor = 2;
    if ((p[1] & 0x7F) == 126) {
      len = read_be(p + cursor, 2);
      cursor += 2;
    } else if ((p[1] & 0x7F) == 127) {
      len = read_be(p + cursor, 8);
      cursor += 8;
    }
    if (len > kMaxPayload) {
      fail("frame exceeds 1 MiB");
      return false;
    }
    if ((opcode & 0x8) && (!fin || len > 125)) {
      fail("illegal control frame");
      return false;
    }
    if (require_mask_ && !masked) {
      fail("client frame was not masked");
      return false;
    }
    std::uint8_t mask[4] = {};
    if (masked) {
      std::memcpy(mask, p + cursor, 4);
      cursor += 4;
    }
    if (avail < header + static_cast<std::size_t>(len)) {
      compact();
      return false;
    }
    std::string payload(static_cast<std::size_t>(len), '\0');
    const std::uint8_t* data = p + cursor;
    for (std::uint64_t i = 0; i < len; ++i) {
      const auto b = data[i];
      payload[static_cast<std::size_t>(i)] =
          masked ? static_cast<char>(b ^ mask[i % 4]) : static_cast<char>(b);
    }
    off_ += header + static_cast<std::size_t>(len);

    const bool control = (opcode & 0x8) != 0;
    if (control) {
      out.opcode = opcode;
      out.payload = std::move(payload);
      return true;
    }
    if (opcode == WS_CONT) {
      if (!in_frag_) {
        fail("unexpected continuation");
        return false;
      }
      if (frag_.size() + payload.size() > kMaxPayload) {
        fail("fragmented message exceeds 1 MiB");
        return false;
      }
      frag_ += payload;
      if (fin) {
        out.opcode = frag_op_;
        out.payload = std::move(frag_);
        frag_.clear();
        in_frag_ = false;
        return true;
      }
      continue;
    }
    if (in_frag_) {
      fail("new data frame during a fragment");
      return false;
    }
    if (!fin) {
      in_frag_ = true;
      frag_op_ = opcode;
      frag_ = std::move(payload);
      continue;
    }
    out.opcode = opcode;
    out.payload = std::move(payload);
    return true;
  }
}

std::string encode_frame(std::uint8_t opcode, std::string_view payload, bool mask) {
  std::string out;
  out.reserve(payload.size() + 14);
  out.push_back(static_cast<char>(0x80 | opcode));
  const std::size_t len = payload.size();
  const std::uint8_t mask_bit = mask ? 0x80 : 0;
  if (len < 126) {
    out.push_back(static_cast<char>(mask_bit | static_cast<std::uint8_t>(len)));
  } else if (len <= 0xFFFF) {
    out.push_back(static_cast<char>(mask_bit | 126));
    out.push_back(static_cast<char>((len >> 8) & 0xFF));
    out.push_back(static_cast<char>(len & 0xFF));
  } else {
    out.push_back(static_cast<char>(mask_bit | 127));
    for (int shift = 56; shift >= 0; shift -= 8) {
      out.push_back(static_cast<char>((static_cast<std::uint64_t>(len) >> shift) & 0xFF));
    }
  }
  if (mask) {
    std::uint8_t key[4];
    std::random_device rd;
    for (auto& b : key) b = static_cast<std::uint8_t>(rd());
    out.append(reinterpret_cast<const char*>(key), 4);
    for (std::size_t i = 0; i < payload.size(); ++i) {
      out.push_back(static_cast<char>(static_cast<std::uint8_t>(payload[i]) ^ key[i % 4]));
    }
  } else {
    out.append(payload.data(), payload.size());
  }
  return out;
}

}  // namespace diskost
