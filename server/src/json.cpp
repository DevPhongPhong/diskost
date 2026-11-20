#include "diskost/json.hpp"

#include <cctype>
#include <locale>
#include <sstream>
#include <stdexcept>
#include <utility>

namespace diskost {
namespace {

class Parser {
 public:
  explicit Parser(std::string_view text) : s_(text) {}

  Json parse() {
    Json value = parse_value();
    skip();
    if (i_ != s_.size()) {
      throw std::runtime_error("trailing data in json");
    }
    return value;
  }

 private:
  std::string_view s_;
  std::size_t i_ = 0;

  void skip() {
    while (i_ < s_.size() && std::isspace(static_cast<unsigned char>(s_[i_]))) {
      ++i_;
    }
  }

  char peek() {
    skip();
    return i_ < s_.size() ? s_[i_] : '\0';
  }

  Json parse_value() {
    const char c = peek();
    if (c == '"') return Json::str(parse_string());
    if (c == '{') return parse_object();
    if (c == '[') return parse_array();
    if (c == 't') return expect_lit("true", Json::boolean(true));
    if (c == 'f') return expect_lit("false", Json::boolean(false));
    if (c == 'n') return expect_lit("null", Json::nul());
    if (c == '-' || std::isdigit(static_cast<unsigned char>(c))) return parse_number();
    throw std::runtime_error("invalid json value");
  }

  Json expect_lit(std::string_view lit, Json value) {
    skip();
    if (s_.substr(i_, lit.size()) != lit) {
      throw std::runtime_error("invalid json literal");
    }
    i_ += lit.size();
    return value;
  }

  Json parse_object() {
    skip();
    if (s_[i_++] != '{') throw std::runtime_error("expected '{'");
    Json obj = Json::object();
    if (peek() == '}') {
      ++i_;
      return obj;
    }
    while (true) {
      if (peek() != '"') throw std::runtime_error("expected object key");
      std::string key = parse_string();
      if (peek() != ':') throw std::runtime_error("expected ':'");
      ++i_;
      obj.set(std::move(key), parse_value());
      const char c = peek();
      if (c == ',') {
        ++i_;
        continue;
      }
      if (c == '}') {
        ++i_;
        break;
      }
      throw std::runtime_error("expected ',' or '}'");
    }
    return obj;
  }

  Json parse_array() {
    skip();
    if (s_[i_++] != '[') throw std::runtime_error("expected '['");
    Json arr = Json::array();
    if (peek() == ']') {
      ++i_;
      return arr;
    }
    while (true) {
      arr.push(parse_value());
      const char c = peek();
      if (c == ',') {
        ++i_;
        continue;
      }
      if (c == ']') {
        ++i_;
        break;
      }
      throw std::runtime_error("expected ',' or ']'");
    }
    return arr;
  }

  std::string parse_string() {
    skip();
    if (i_ >= s_.size() || s_[i_++] != '"') throw std::runtime_error("expected string");
    std::string out;
    while (i_ < s_.size()) {
      const unsigned char c = static_cast<unsigned char>(s_[i_++]);
      if (c == '"') return out;
      if (c == '\\') {
        if (i_ >= s_.size()) throw std::runtime_error("bad escape");
        const char e = s_[i_++];
        switch (e) {
          case '"':
          case '\\':
          case '/':
            out.push_back(e);
            break;
          case 'b':
            out.push_back('\b');
            break;
          case 'f':
            out.push_back('\f');
            break;
          case 'n':
            out.push_back('\n');
            break;
          case 'r':
            out.push_back('\r');
            break;
          case 't':
            out.push_back('\t');
            break;
          case 'u': {
            if (i_ + 4 > s_.size()) throw std::runtime_error("short unicode escape");
            int cp = 0;
            for (int k = 0; k < 4; ++k) {
              const char h = s_[i_++];
              cp <<= 4;
              if (h >= '0' && h <= '9') cp += h - '0';
              else if (h >= 'a' && h <= 'f') cp += h - 'a' + 10;
              else if (h >= 'A' && h <= 'F') cp += h - 'A' + 10;
              else throw std::runtime_error("bad unicode escape");
            }
            append_utf8(out, cp);
            break;
          }
          default:
            throw std::runtime_error("bad escape");
        }
      } else if (c < 0x20) {
        throw std::runtime_error("control character in string");
      } else {
        out.push_back(static_cast<char>(c));
      }
    }
    throw std::runtime_error("unterminated string");
  }

  static void append_utf8(std::string& out, int cp) {
    if (cp < 0 || cp > 0x10FFFF) throw std::runtime_error("bad code point");
    if (cp < 0x80) {
      out.push_back(static_cast<char>(cp));
    } else if (cp < 0x800) {
      out.push_back(static_cast<char>(0xC0 | (cp >> 6)));
      out.push_back(static_cast<char>(0x80 | (cp & 0x3F)));
    } else if (cp < 0x10000) {
      out.push_back(static_cast<char>(0xE0 | (cp >> 12)));
      out.push_back(static_cast<char>(0x80 | ((cp >> 6) & 0x3F)));
      out.push_back(static_cast<char>(0x80 | (cp & 0x3F)));
    } else {
      out.push_back(static_cast<char>(0xF0 | (cp >> 18)));
      out.push_back(static_cast<char>(0x80 | ((cp >> 12) & 0x3F)));
      out.push_back(static_cast<char>(0x80 | ((cp >> 6) & 0x3F)));
      out.push_back(static_cast<char>(0x80 | (cp & 0x3F)));
    }
  }

  Json parse_number() {
    skip();
    const std::size_t start = i_;
    if (s_[i_] == '-') ++i_;
    if (i_ >= s_.size() || !std::isdigit(static_cast<unsigned char>(s_[i_]))) {
      throw std::runtime_error("invalid number");
    }
    if (s_[i_] == '0') {
      ++i_;
    } else {
      while (i_ < s_.size() && std::isdigit(static_cast<unsigned char>(s_[i_]))) ++i_;
    }
    bool is_float = false;
    if (i_ < s_.size() && s_[i_] == '.') {
      is_float = true;
      ++i_;
      if (i_ >= s_.size() || !std::isdigit(static_cast<unsigned char>(s_[i_]))) {
        throw std::runtime_error("invalid fraction");
      }
      while (i_ < s_.size() && std::isdigit(static_cast<unsigned char>(s_[i_]))) ++i_;
    }
    if (i_ < s_.size() && (s_[i_] == 'e' || s_[i_] == 'E')) {
      is_float = true;
      ++i_;
      if (i_ < s_.size() && (s_[i_] == '+' || s_[i_] == '-')) ++i_;
      if (i_ >= s_.size() || !std::isdigit(static_cast<unsigned char>(s_[i_]))) {
        throw std::runtime_error("invalid exponent");
      }
      while (i_ < s_.size() && std::isdigit(static_cast<unsigned char>(s_[i_]))) ++i_;
    }
    const std::string token(s_.substr(start, i_ - start));
    if (!is_float) {
      try {
        std::size_t used = 0;
        const long long v = std::stoll(token, &used, 10);
        if (used == token.size()) return Json::number(static_cast<std::int64_t>(v));
      } catch (const std::exception&) {
        // Values outside int64 are stored as doubles.
      }
    }
    std::istringstream in(token);
    in.imbue(std::locale::classic());
    double dv = 0;
    in >> dv;
    if (!in || !in.eof()) throw std::runtime_error("invalid number");
    return Json::from_double(dv);
  }

  friend class Json;
};

void dump_string(std::ostream& out, std::string_view s) {
  out << '"';
  for (unsigned char c : s) {
    switch (c) {
      case '"':
        out << "\\\"";
        break;
      case '\\':
        out << "\\\\";
        break;
      case '\b':
        out << "\\b";
        break;
      case '\f':
        out << "\\f";
        break;
      case '\n':
        out << "\\n";
        break;
      case '\r':
        out << "\\r";
        break;
      case '\t':
        out << "\\t";
        break;
      default:
        if (c < 0x20) {
          const char* hex = "0123456789abcdef";
          out << "\\u00" << hex[c >> 4] << hex[c & 0xF];
        } else {
          out << static_cast<char>(c);
        }
    }
  }
  out << '"';
}

}  // namespace

Json Json::from_double(double value) {
  Json j;
  j.type_ = Type::Number;
  j.is_int_ = false;
  j.float_ = value;
  return j;
}

Json Json::nul() { return Json(); }

Json Json::boolean(bool value) {
  Json j;
  j.type_ = Type::Bool;
  j.bool_ = value;
  return j;
}

Json Json::number(std::int64_t value) {
  Json j;
  j.type_ = Type::Number;
  j.is_int_ = true;
  j.int_ = value;
  j.float_ = static_cast<double>(value);
  return j;
}

Json Json::str(std::string value) {
  Json j;
  j.type_ = Type::String;
  j.string_ = std::move(value);
  return j;
}

Json Json::array() {
  Json j;
  j.type_ = Type::Array;
  return j;
}

Json Json::object() {
  Json j;
  j.type_ = Type::Object;
  return j;
}

Json Json::parse(std::string_view text) { return Parser(text).parse(); }

bool Json::as_bool() const {
  if (type_ != Type::Bool) throw std::runtime_error("not a bool");
  return bool_;
}

std::int64_t Json::as_int() const {
  if (type_ != Type::Number) throw std::runtime_error("not a number");
  if (is_int_) return int_;
  return static_cast<std::int64_t>(float_);
}

const std::string& Json::as_string() const {
  if (type_ != Type::String) throw std::runtime_error("not a string");
  return string_;
}

const std::vector<Json>& Json::as_array() const {
  if (type_ != Type::Array) throw std::runtime_error("not an array");
  return array_;
}

const Json* Json::get(std::string_view key) const {
  if (type_ != Type::Object) return nullptr;
  for (const auto& entry : object_) {
    if (entry.first == key) return &entry.second;
  }
  return nullptr;
}

std::string Json::get_string(std::string_view key) const {
  const Json* v = get(key);
  if (!v || !v->is_string()) return {};
  return v->as_string();
}

std::int64_t Json::get_int(std::string_view key, std::int64_t fallback) const {
  const Json* v = get(key);
  if (!v || !v->is_number()) return fallback;
  return v->as_int();
}

bool Json::get_bool(std::string_view key, bool fallback) const {
  const Json* v = get(key);
  if (!v || !v->is_bool()) return fallback;
  return v->as_bool();
}

void Json::set(std::string key, Json value) {
  if (type_ != Type::Object) throw std::runtime_error("set on non-object");
  for (auto& entry : object_) {
    if (entry.first == key) {
      entry.second = std::move(value);
      return;
    }
  }
  object_.emplace_back(std::move(key), std::move(value));
}

void Json::push(Json value) {
  if (type_ != Type::Array) throw std::runtime_error("push on non-array");
  array_.push_back(std::move(value));
}

std::string Json::dump() const {
  std::ostringstream out;
  out.imbue(std::locale::classic());
  switch (type_) {
    case Type::Null:
      out << "null";
      break;
    case Type::Bool:
      out << (bool_ ? "true" : "false");
      break;
    case Type::Number:
      if (is_int_) {
        out << int_;
      } else {
        out.setf(std::ios::fmtflags(0), std::ios::floatfield);
        out.precision(15);
        out << float_;
      }
      break;
    case Type::String:
      dump_string(out, string_);
      break;
    case Type::Array: {
      out << '[';
      for (std::size_t i = 0; i < array_.size(); ++i) {
        if (i) out << ',';
        out << array_[i].dump();
      }
      out << ']';
      break;
    }
    case Type::Object: {
      out << '{';
      for (std::size_t i = 0; i < object_.size(); ++i) {
        if (i) out << ',';
        dump_string(out, object_[i].first);
        out << ':' << object_[i].second.dump();
      }
      out << '}';
      break;
    }
  }
  return out.str();
}

}  // namespace diskost
