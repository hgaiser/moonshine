#pragma once
#include <string>
#include <variant>

namespace inputtino {

struct Error {
  explicit Error(std::string s) : msg(std::move(s)) {}
  std::string msg;
};

template <typename T> class Result {
public:
  Result(const Error &errorString) : contents(errorString) {}

  Result(T &&value) : contents(std::move(value)) {}

  explicit operator bool() const {
    return contents.index() == 0;
  }

  const T &operator*() const {
    return std::get<T>(contents);
  }

  T &operator*() {
    return std::get<T>(contents);
  }

  std::string getErrorMessage() {
    return std::get<Error>(contents).msg;
  }

private:
  std::variant<T, Error> contents;
};
} // namespace inputtino