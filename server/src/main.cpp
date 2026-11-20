#include "diskost/server.hpp"

#include <cstdint>
#include <cstdlib>
#include <iostream>
#include <string>
#include <vector>

namespace {

void usage() {
  std::cerr
      << "diskost-server — meeting signaling server\n"
      << "Usage: diskost-server [--host 0.0.0.0] [--port 7880]\n"
      << "                      [--max-participants 16] [--max-rooms 1000]\n"
      << "                      [--stun URL] [--no-stun] [--verbose]\n"
      << "\n"
      << "WebSocket endpoint: ws://<host>:<port>/ws\n"
      << "Health check:       http://<host>:<port>/health\n";
}

}  // namespace

int main(int argc, char** argv) {
  std::string host = "0.0.0.0";
  int port = 7880;
  diskost::ServerConfig config;
  bool stun_set = false;
  std::vector<std::string> stun;

  for (int i = 1; i < argc; ++i) {
    const std::string arg = argv[i];
    auto need = [&](const char* name) -> const char* {
      if (i + 1 >= argc) {
        std::cerr << "missing value for " << name << "\n";
        std::exit(2);
      }
      return argv[++i];
    };
    if (arg == "--help" || arg == "-h") {
      usage();
      return 0;
    } else if (arg == "--host") {
      host = need("--host");
    } else if (arg == "--port") {
      port = std::stoi(need("--port"));
    } else if (arg == "--max-participants") {
      config.max_participants = std::stoi(need("--max-participants"));
    } else if (arg == "--max-rooms") {
      config.max_rooms = std::stoi(need("--max-rooms"));
    } else if (arg == "--stun") {
      stun_set = true;
      stun.emplace_back(need("--stun"));
    } else if (arg == "--no-stun") {
      stun_set = true;
    } else if (arg == "--verbose") {
      config.verbose = true;
    } else {
      std::cerr << "unknown argument: " << arg << "\n";
      usage();
      return 2;
    }
  }
  if (stun_set) config.ice_servers = std::move(stun);
  if (port < 0 || port > 65535 || config.max_participants < 2 || config.max_rooms < 1) {
    std::cerr << "invalid numeric option\n";
    return 2;
  }

  try {
    diskost::MeetingServer server(std::move(config));
    server.serve(host, static_cast<std::uint16_t>(port));
  } catch (const std::exception& ex) {
    std::cerr << "diskost-server: " << ex.what() << "\n";
    return 1;
  }
  return 0;
}
