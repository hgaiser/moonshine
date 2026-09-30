#pragma once

#include <errno.h>
#include <fcntl.h>
#include <functional>
#include <inputtino/result.hpp>
#include <iostream>
#include <linux/uhid.h>
#include <memory>
#include <poll.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <string>
#include <termios.h>
#include <thread>
#include <unistd.h>
#include <vector>

namespace uhid {
struct ThreadState {
  int fd;
  std::function<void(const uhid_event &ev, int fd)> on_event;
  bool stop_repeat_thread = false;
};

struct DeviceDefinition {
  std::string name;
  std::string phys;
  std::string uniq;
  uint16_t bus;
  uint32_t vendor;
  uint32_t product;
  uint32_t version;
  uint32_t country;
  std::vector<unsigned char> report_description;
};

static inputtino::Result<bool> uhid_write(int fd, const struct uhid_event *ev) {
  ssize_t ret = write(fd, ev, sizeof(*ev));
  if (ret < 0) {
    return inputtino::Error(strerror(errno));
  } else if (ret != sizeof(*ev)) {
    return inputtino::Error(strerror(-EFAULT));
  } else {
    return ret;
  }
}

class Device {
private:
  Device(std::shared_ptr<std::thread> ev_thread, std::shared_ptr<ThreadState> state)
      : ev_thread(std::move(ev_thread)), state(std::move(state)) {};
  std::shared_ptr<std::thread> ev_thread;
  std::shared_ptr<ThreadState> state;
  std::shared_ptr<std::function<void(const uhid_event &ev, int fd)>> on_event;

public:
  static inputtino::Result<Device> create(const DeviceDefinition &definition,
                                          const std::function<void(const uhid_event &ev, int fd)> &on_event);

  Device(Device &&j) noexcept : ev_thread(nullptr), state(nullptr), on_event(nullptr) {
    std::swap(j.ev_thread, ev_thread);
    std::swap(j.state, state);
    std::swap(j.on_event, on_event);
  }

  Device(Device const &) = delete;
  Device &operator=(Device const &) = delete;

  inline inputtino::Result<bool> send(const uhid_event &ev) {
    return uhid_write(state->fd, &ev);
  }

  inline void stop_thread() {
    state->stop_repeat_thread = true;
    if (ev_thread->joinable()) {
      ev_thread->join(); // let's wait for the thread to finish
    }
  }

  ~Device() {
    if (state) {
      struct uhid_event ev{};
      ev.type = UHID_DESTROY;
      uhid_write(state->fd, &ev);

      close(state->fd); // This should also close the thread by causing a POLLHUP

      state->stop_repeat_thread = true;
      if (ev_thread->joinable()) {
        ev_thread->join(); // let's wait for the thread to finish
      }
    }
  }
};

static void set_c_str(const std::string &str, unsigned char *c_str) {
  std::copy(str.begin(), str.end(), c_str);
  c_str[str.length()] = 0;
}

constexpr int UHID_POLL_TIMEOUT = 500; // ms

inputtino::Result<Device> Device::create(const DeviceDefinition &definition,
                                         const std::function<void(const uhid_event &ev, int fd)> &on_event) {

  int fd = open("/dev/uhid", O_RDWR | O_CLOEXEC);
  if (fd < 0) {
    return inputtino::Error(strerror(errno));
  }

  auto ev = uhid_event{};
  ev.type = UHID_CREATE2, ev.u.create2.bus = definition.bus, ev.u.create2.vendor = definition.vendor,
  ev.u.create2.product = definition.product, ev.u.create2.version = definition.version,
  ev.u.create2.country = definition.country,
  ev.u.create2.rd_size = static_cast<std::uint16_t>(definition.report_description.size()),
  std::copy(definition.report_description.begin(), definition.report_description.end(), ev.u.create2.rd_data);
  set_c_str(definition.name, ev.u.create2.name);
  set_c_str(definition.phys, ev.u.create2.phys);
  set_c_str(definition.uniq, ev.u.create2.uniq);

  auto res = uhid_write(fd, &ev);
  if (res) {
    auto state = std::make_shared<ThreadState>();
    state->fd = fd;
    state->on_event = on_event;
    auto thread = std::make_shared<std::thread>([state]() {
      std::array<pollfd, 1> pfds = {pollfd{.fd = state->fd, .events = POLLIN}};
      int poll_rs = 0;

      while (!state->stop_repeat_thread) {
        poll_rs = poll(pfds.data(), pfds.size(), UHID_POLL_TIMEOUT);
        if (poll_rs < 0) {
          std::cerr << "Failed polling uhid fd; ret=" << strerror(errno) << std::endl;
          break;
        }
        if (pfds[0].revents & POLLHUP) {
          std::cerr << "HUP on uhid-cdev" << std::endl;
          break;
        }
        if (pfds[0].revents & POLLIN) {
          struct uhid_event ev{};
          auto ret = read(state->fd, &ev, sizeof(ev));
          if (ret == 0) {
            std::cerr << "Read HUP on uhid-cdev" << std::endl;
          } else if (ret < 0) {
            std::cerr << "Cannot read uhid-cdev: " << strerror(errno) << std::endl;
          } else if (ret != sizeof(ev)) {
            std::cerr << "Invalid size read from uhid-dev" << ret << " != " << sizeof(ev) << std::endl;
          } else {
            if (state->on_event) {
              state->on_event(ev, state->fd);
            }
          }
        }
      }
    });
    thread->detach();
    return inputtino::Result<Device>({std::move(thread), std::move(state)});
  } else {
    close(fd);
    return inputtino::Error(res.getErrorMessage());
  }
}

} // namespace uhid
