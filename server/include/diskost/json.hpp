#pragma once

#include <cstdint>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

namespace diskost {

// JSON subset used by the meeting protocol: null, bool, int/float, string,
// array, and object. Objects keep insertion order.
class Json {
 public:
  enum class Type { Null, Bool, Number, String, Array, Object };

  static Json nul();
  static Json boolean(bool value);
  static Json number(std::int64_t value);
  static Json from_double(double value);
  static Json str(std::string value);
  static Json array();
  static Json object();

  static Json parse(std::string_view text);

  Type type() const { return type_; }
  bool is_null() const { return type_ == Type::Null; }
  bool is_bool() const { return type_ == Type::Bool; }
  bool is_number() const { return type_ == Type::Number; }
  bool is_string() const { return type_ == Type::String; }
  bool is_array() const { return type_ == Type::Array; }
  bool is_object() const { return type_ == Type::Object; }

  bool as_bool() const;
  std::int64_t as_int() const;
  const std::string& as_string() const;
  const std::vector<Json>& as_array() const;

  // Missing keys and type mismatches return the fallback.
  const Json* get(std::string_view key) const;
  std::string get_string(std::string_view key) const;
  std::int64_t get_int(std::string_view key, std::int64_t fallback = 0) const;
  bool get_bool(std::string_view key, bool fallback = false) const;

  void set(std::string key, Json value);
  void push(Json value);

  std::string dump() const;

 private:
  Type type_ = Type::Null;
  bool bool_ = false;
  bool is_int_ = false;
  std::int64_t int_ = 0;
  double float_ = 0;
  std::string string_;
  std::vector<Json> array_;
  std::vector<std::pair<std::string, Json>> object_;
};

}  // namespace diskost
