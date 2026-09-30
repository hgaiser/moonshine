#pragma once

#include <cstring>
#include <inputtino/input.hpp>

static char **c_get_nodes(void *device, int *num_nodes) {
  char **nodes = nullptr;
  *num_nodes = 0;
  if (device) {
    auto devices = reinterpret_cast<inputtino::VirtualDevice *>(device)->get_nodes();
    *num_nodes = devices.size();
    nodes = new char *[devices.size()];
    for (int index = 0; index < devices.size(); index++) {
      nodes[index] = new char[devices[index].size() + 1];
      strcpy(nodes[index], devices[index].c_str());
    }
  }
  return nodes;
}
