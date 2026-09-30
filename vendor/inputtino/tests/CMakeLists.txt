include(FetchContent)

# Testing library
FetchContent_Declare(
        Catch2
        GIT_REPOSITORY https://github.com/catchorg/Catch2.git
        GIT_TAG v3.3.2
)

FetchContent_MakeAvailable(Catch2)

# Tests need to be added as executables first
add_executable(inputtino_tests main.cpp)

set(SRC_LIST main.cpp testCAPI.cpp)

if (UNIX AND NOT APPLE)
    option(TEST_LIBINPUT "Enable libinput test" ON)
    if (TEST_LIBINPUT)
        find_package(PkgConfig)
        pkg_check_modules(LIBINPUT REQUIRED IMPORTED_TARGET libinput)
        option(SDL_CUSTOM_SRC "Use a custom SDL source location (useful to better debug)" OFF)
        if (SDL_CUSTOM_SRC)
            SET(SDL_TEST OFF)
            SET(SDL_HIDAPI_JOYSTICK ON)
            SET(SDL_PIPEWIRE OFF)
            add_subdirectory(${SDL_CUSTOM_SRC} ${CMAKE_CURRENT_BINARY_DIR}/sdl EXCLUDE_FROM_ALL)
        else ()
            find_package(SDL2 REQUIRED CONFIG REQUIRED COMPONENTS SDL2)
        endif ()

        target_link_libraries(inputtino_tests PRIVATE
                PkgConfig::LIBINPUT
                SDL2::SDL2)

        list(APPEND SRC_LIST
                "testJoypads.cpp"
                "testLibinput.cpp")

        if (USE_UHID)
            target_compile_definitions(inputtino_tests PRIVATE USE_UHID)
            list(APPEND SRC_LIST "testUHID.cpp")
        endif ()
    endif ()
endif ()

option(TEST_SERVER "Enable server test" OFF)
if (TEST_SERVER)
    list(APPEND SRC_LIST "testServer.cpp")
    target_link_libraries(inputtino_tests PRIVATE inputtino::server)
endif ()

target_sources(inputtino_tests PRIVATE ${SRC_LIST})

# I'm using C++17 in the test
target_compile_features(inputtino_tests PRIVATE cxx_std_17)

# Should be linked to the main library, as well as the Catch2 testing library
target_link_libraries(inputtino_tests PRIVATE
        inputtino::libinputtino
        Catch2::Catch2WithMain)

# See: https://github.com/catchorg/Catch2/blob/devel/docs/cmake-integration.md
list(APPEND CMAKE_MODULE_PATH ${catch2_SOURCE_DIR}/extras)
include(CTest)
include(Catch)
catch_discover_tests(inputtino_tests)
