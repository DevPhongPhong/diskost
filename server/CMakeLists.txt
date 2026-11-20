cmake_minimum_required(VERSION 3.16)
project(diskost-server LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

find_package(Threads REQUIRED)

add_library(diskost_server STATIC
  src/json.cpp
  src/sha1.cpp
  src/net.cpp
  src/ws.cpp
  src/server.cpp
)
target_include_directories(diskost_server PUBLIC include)
target_link_libraries(diskost_server PUBLIC Threads::Threads)
if(WIN32)
  target_link_libraries(diskost_server PUBLIC ws2_32)
endif()

if(MSVC)
  target_compile_options(diskost_server PRIVATE /W4 /permissive-)
else()
  target_compile_options(diskost_server PRIVATE -Wall -Wextra -Wpedantic)
endif()

add_executable(diskost-server src/main.cpp)
target_link_libraries(diskost-server PRIVATE diskost_server)

add_executable(diskost-server-tests tests/server_test.cpp)
target_link_libraries(diskost-server-tests PRIVATE diskost_server)

enable_testing()
add_test(NAME server_tests COMMAND diskost-server-tests)
