#include "catch2/catch_all.hpp"
#include <filesystem>
#include <immer/atom.hpp>
#include <server/json_serialization.hpp>
#include <server/rest.hpp>

using namespace Catch::Matchers;

class HTTPServerFixture {
private:
  std::shared_ptr<httplib::Server> server;
  std::thread server_thread;
  std::shared_ptr<immer::atom<ServerState>> state;

public:
  std::string rest_ip;
  int rest_port;

  HTTPServerFixture() : rest_ip("127.0.0.1"), rest_port(19999) {
    auto local_state = ServerState{.devices = {}};
    state = std::make_shared<immer::atom<ServerState>>(local_state);
    server = std::shared_ptr<httplib::Server>(setup_rest_server(state).release());
    server_thread =
        std::thread([svr = server, ip = this->rest_ip, port = this->rest_port]() { svr->listen(ip, port); });
    std::this_thread::sleep_for(std::chrono::milliseconds(1)); // wait for server to come online
  }

  ~HTTPServerFixture() {
    // Gracefully stop
    server->stop();
    server_thread.join();
  }
};

TEST_CASE("Test JSON serialization", "[server]") {
  auto state = ServerState{
      .devices = {{0,
                   LocalDevice{.type = DeviceType::MOUSE,
                               .client_id = "ID1",
                               .device = std::make_shared<inputtino::Mouse>(std::move(*inputtino::Mouse::create()))}}}};

  auto payload = json(state).dump();
  auto parsed_payload = json::parse(payload);
  REQUIRE(parsed_payload["devices"].size() == 1);
  parsed_payload = parsed_payload["devices"][0];
  REQUIRE_THAT(parsed_payload["type"], Equals("MOUSE"));
  REQUIRE_THAT(parsed_payload["client_id"], Equals("ID1"));
  REQUIRE_THAT(parsed_payload["device_id"], Equals("0"));
  REQUIRE(parsed_payload["device_nodes"].size() >= 1);
}

TEST_CASE_METHOD(HTTPServerFixture, "Test REST server", "[server]") {
  httplib::Client client(this->rest_ip, this->rest_port);

  { // Test GET /devices
    auto res = client.Get("/api/v1.0/devices");
    REQUIRE(res);
    REQUIRE(res->status == 200);
    REQUIRE_THAT(res->body, Equals("{\"devices\":[]}")); // it starts empty
  }

  { // Test full device lifecycle
    // Test POST /devices/add (mouse)
    auto res = client.Post("/api/v1.0/devices/add", json{{"type", "MOUSE"}}.dump(), "application/json");
    REQUIRE(res);
    REQUIRE(res->status == 200);
    auto new_device = json::parse(res->body);
    std::string device_id = new_device["device_id"];
    REQUIRE_THAT(new_device["client_id"], Equals("127.0.0.1"));
    REQUIRE(new_device["device_nodes"].size() >= 1);
    REQUIRE_THAT(new_device["type"], Equals("MOUSE"));
    REQUIRE(!device_id.empty());
    // Are the nodes actually created on the host?
    for (auto node : new_device["device_nodes"]) {
      REQUIRE(std::filesystem::exists(node));
    }

    // Test that GET /devices lists the new device
    res = client.Get("/api/v1.0/devices");
    REQUIRE(res);
    REQUIRE(res->status == 200);
    auto devices = json::parse(res->body);
    REQUIRE(devices["devices"].size() == 1);
    devices = devices["devices"][0];
    REQUIRE_THAT((std::vector<std::string>)devices["device_nodes"],
                 Equals((std::vector<std::string>)new_device["device_nodes"]));
    REQUIRE_THAT(devices["client_id"], Equals(new_device["client_id"]));
    REQUIRE_THAT(devices["type"], Equals(new_device["type"]));
    REQUIRE(devices["device_id"] == new_device["device_id"]);

    // Test that we can move the mouse
    res = client.Post("/api/v1.0/devices/mouse/" + device_id + "/move_rel",
                      json{{"delta_x", 100}, {"delta_y", 100}}.dump(),
                      "application/json");
    REQUIRE(res);
    REQUIRE(res->status == 200);
    // TODO: check with libinput that the mouse actually moved

    // Test that DELETE /devices/<device_id> removes the device
    res = client.Delete("/api/v1.0/devices/" + device_id);
    REQUIRE(res);
    REQUIRE(res->status == 200);
    res = client.Get("/api/v1.0/devices");
    REQUIRE(json::parse(res->body)["devices"].empty());
    // Are the nodes actually removed from the host?
    for (auto node : new_device["device_nodes"]) {
      REQUIRE(!std::filesystem::exists(node));
    }
  }

  { // Test keyboard
    // Test POST /devices/add (keyboard)
    auto res = client.Post("/api/v1.0/devices/add", json{{"type", "KEYBOARD"}}.dump(), "application/json");
    REQUIRE(res);
    REQUIRE(res->status == 200);
    auto new_device = json::parse(res->body);
    std::string device_id = new_device["device_id"];
    REQUIRE_THAT(new_device["client_id"], Equals("127.0.0.1"));
    REQUIRE(new_device["device_nodes"].size() == 1);
    REQUIRE_THAT(new_device["type"], Equals("KEYBOARD"));
    REQUIRE(!device_id.empty());
    // Are the nodes actually created on the host?
    for (auto node : new_device["device_nodes"]) {
      REQUIRE(std::filesystem::exists(node));
    }

    // Test that GET /devices lists the new device
    res = client.Get("/api/v1.0/devices");
    REQUIRE(res);
    REQUIRE(res->status == 200);
    auto devices = json::parse(res->body);
    REQUIRE(devices["devices"].size() == 1);
    devices = devices["devices"][0];
    REQUIRE_THAT((std::vector<std::string>)devices["device_nodes"],
                 Equals((std::vector<std::string>)new_device["device_nodes"]));
    REQUIRE_THAT(devices["client_id"], Equals(new_device["client_id"]));
    REQUIRE_THAT(devices["type"], Equals(new_device["type"]));
    REQUIRE(devices["device_id"] == new_device["device_id"]);

    // Test that we can move the mouse
    res = client.Post("/api/v1.0/devices/keyboard/" + device_id + "/press",
                      json{{"key", 41}} // Pressing A
                          .dump(),
                      "application/json");
    REQUIRE(res);
    REQUIRE(res->status == 200);
    // TODO: check with libinput that the keyboard pressed A

    // Test that DELETE /devices/<device_id> removes the device
    res = client.Delete("/api/v1.0/devices/" + device_id);
    REQUIRE(res);
    REQUIRE(res->status == 200);
    res = client.Get("/api/v1.0/devices");
    REQUIRE(json::parse(res->body)["devices"].empty());
    // Are the nodes actually removed from the host?
    // This might take a few millis since it's stopping the background thread
    std::this_thread::sleep_for(std::chrono::milliseconds(100));
    for (auto node : new_device["device_nodes"]) {
      REQUIRE(!std::filesystem::exists(node));
    }
  }

  { // Test POST /devices/add without type
    auto res = client.Post("/api/v1.0/devices/add", "{}", "application/json");
    REQUIRE(res);
    REQUIRE(res->status == 500);
    REQUIRE_THAT(res->body, ContainsSubstring("key 'type' not found"));
  }
}