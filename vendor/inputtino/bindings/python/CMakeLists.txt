cmake_minimum_required(VERSION 3.15)
project(${SKBUILD_PROJECT_NAME} LANGUAGES CXX)

set(CMAKE_POSITION_INDEPENDENT_CODE ON)

set(PYBIND11_FINDPYTHON ON)
find_package(pybind11 CONFIG REQUIRED)

# add inputtino
get_filename_component(PROJECT_ROOT "${CMAKE_CURRENT_SOURCE_DIR}/../.." ABSOLUTE)
add_subdirectory(${PROJECT_ROOT} "${CMAKE_CURRENT_BINARY_DIR}/inputtino")

execute_process(
    COMMAND pybind11-config --includes
    OUTPUT_VARIABLE PYBIND11_INCLUDES
    OUTPUT_STRIP_TRAILING_WHITESPACE
)

pybind11_add_module(_core MODULE src/main.cpp)
target_include_directories(_core PRIVATE ${PYBIND11_INCLUDES})
target_link_libraries(_core PUBLIC inputtino::libinputtino)

install(TARGETS _core DESTINATION inputtino)
