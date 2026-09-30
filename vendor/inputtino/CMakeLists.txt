cmake_minimum_required(VERSION 3.13...3.24)

# Project name and a few useful settings. Other commands can pick up the results
project(inputtino
        VERSION 0.1
        DESCRIPTION "A virtual input library"
        LANGUAGES C CXX)

#----------------------------------------------------------------------------------------------------------------------
# general settings and options
#----------------------------------------------------------------------------------------------------------------------

include(cmake/utils.cmake)
include(GNUInstallDirs)
include(CMakePackageConfigHelpers)

# Only do these if this is the main project, and not if it is included through add_subdirectory
if (CMAKE_PROJECT_NAME STREQUAL PROJECT_NAME)

    # Optionally set things like CMAKE_CXX_STANDARD, CMAKE_POSITION_INDEPENDENT_CODE here

    set(CMAKE_CXX_STANDARD 17)

    set(CMAKE_CXX_STANDARD_REQUIRED ON)

    # Let's ensure -std=c++xx instead of -std=g++xx
    set(CMAKE_CXX_EXTENSIONS OFF)

    # Let's nicely support folders in IDEs
    set_property(GLOBAL PROPERTY USE_FOLDERS ON)

    option(BUILD_SHARED_LIBS "Build libs as shared" ON)

    include(CTest)

    set(CMAKE_ARCHIVE_OUTPUT_DIRECTORY "${PROJECT_BINARY_DIR}")
    set(CMAKE_LIBRARY_OUTPUT_DIRECTORY "${PROJECT_BINARY_DIR}")
    set(CMAKE_RUNTIME_OUTPUT_DIRECTORY "${PROJECT_BINARY_DIR}")

    if (NOT DEFINED CMAKE_BUILD_TYPE AND NOT DEFINED CMAKE_CONFIGURATION_TYPES)
        set(CMAKE_BUILD_TYPE Release CACHE STRING "Build type" FORCE)
        set_property(CACHE CMAKE_BUILD_TYPE PROPERTY STRINGS "Debug" "Release" "MinSizeRel" "RelWithDebInfo")
    endif ()
endif ()

# Make an automatic library - will be static or dynamic based on user setting
add_library(libinputtino)
add_library(inputtino::libinputtino ALIAS libinputtino)

# All users of this library will need at least C++17
target_compile_features(libinputtino PUBLIC cxx_std_17)
set_target_properties(libinputtino PROPERTIES LINKER_LANGUAGE CXX)
target_compile_definitions(libinputtino PUBLIC "$<$<NOT:$<BOOL:${BUILD_SHARED_LIBS}>>:INPUTTINO_STATIC_DEFINE>")

#----------------------------------------------------------------------------------------------------------------------
# Export symbols
#----------------------------------------------------------------------------------------------------------------------

include(GenerateExportHeader)
set(export_file_name "export_shared.h")

if (NOT BUILD_SHARED_LIBS)
    set(export_file_name "export_static.h")
endif ()

message(STATUS "Export file name: ${export_file_name}")

generate_export_header(libinputtino EXPORT_FILE_NAME include/inputtino/${export_file_name})


#----------------------------------------------------------------------------------------------------------------------
# Options
#----------------------------------------------------------------------------------------------------------------------

option(BUILD_C_BINDINGS "Build C bindings" OFF)
option(LIBINPUTTINO_INSTALL "Generate the install target" OFF)
option(USE_UHID "Add uhid implementation for advanced DualSense features" ON)

#----------------------------------------------------------------------------------------------------------------------
# Dependencies
#----------------------------------------------------------------------------------------------------------------------

if (UNIX AND NOT APPLE)
    option(LIBEVDEV_CUSTOM_INCLUDE_DIR "Location to a custom libevdev source" OFF)
    option(LIBEVDEV_CUSTOM_LIBRARY "Location to a custom libevdev library" OFF)
    if (LIBEVDEV_CUSTOM_INCLUDE_DIR AND LIBEVDEV_CUSTOM_LIBRARY)
        target_include_directories(libinputtino PUBLIC "${LIBEVDEV_CUSTOM_INCLUDE_DIR}")
        target_link_libraries(libinputtino PUBLIC "${LIBEVDEV_CUSTOM_LIBRARY}")
    else ()
        find_package(PkgConfig)
        pkg_check_modules(LIBEVDEV REQUIRED IMPORTED_TARGET libevdev)
        target_link_libraries(libinputtino PUBLIC PkgConfig::LIBEVDEV)
    endif ()
endif ()

find_program(CCACHE_FOUND ccache)
if (CCACHE_FOUND)
    set_property(GLOBAL PROPERTY RULE_LAUNCH_COMPILE ccache)
endif (CCACHE_FOUND)

#----------------------------------------------------------------------------------------------------------------------
# Sources
#----------------------------------------------------------------------------------------------------------------------

set(PUBLIC_HEADERS
        include/inputtino/input.hpp
        include/inputtino/result.hpp
        include/inputtino/input.h)

if (UNIX AND NOT APPLE)
    list(APPEND SRC_LIST
            "src/uinput/joypad_nintendo.cpp"
            "src/uinput/joypad_xbox.cpp"
            "src/uinput/keyboard.cpp"
            "src/uinput/mouse.cpp"
            "src/uinput/pentablet.cpp"
            "src/uinput/touchscreen.cpp"
            "src/uinput/trackpad.cpp")

    if (USE_UHID)
        message("Using uhid implementation for DualSense controller")
        list(APPEND SRC_LIST "src/uhid/joypad_ps5.cpp")
        target_include_directories(libinputtino PUBLIC "src/uhid/include/")
    else ()
        message("Using uinput implementation for DualSense controller")
        list(APPEND SRC_LIST "src/uinput/joypad_ps.cpp")
    endif ()

    target_sources(libinputtino PRIVATE ${SRC_LIST})
    target_include_directories(libinputtino PUBLIC "src/uinput/include")
endif ()

if (BUILD_C_BINDINGS)
    file(GLOB C_SRC_FILES SRCS src/c-bindings/*.cpp)
    target_sources(libinputtino PRIVATE
            src/c-bindings/helpers.hpp
            ${C_SRC_FILES})
endif ()

# Testing only available if this is the main app
# Emergency override MODERN_CMAKE_BUILD_TESTING provided as well
if ((CMAKE_PROJECT_NAME STREQUAL PROJECT_NAME OR MODERN_CMAKE_BUILD_TESTING)
        AND BUILD_TESTING)
    add_subdirectory(tests)
endif ()

#----------------------------------------------------------------------------------------------------------------------
# Install
#----------------------------------------------------------------------------------------------------------------------

# Only do these if this is the main project, and not if it is included through add_subdirectory
if (CMAKE_PROJECT_NAME STREQUAL PROJECT_NAME)
    # Only export explicitly defined symbols
    set_if_undefined(CMAKE_CXX_VISIBILITY_PRESET hidden)
    set_if_undefined(CMAKE_VISIBILITY_INLINES_HIDDEN ON)

    if (LIBINPUTTINO_INSTALL AND NOT CMAKE_SKIP_INSTALL_RULES)
        install(TARGETS libinputtino
                EXPORT libinputtino_export
                RUNTIME COMPONENT libinputtino
                LIBRARY COMPONENT libinputtino
                NAMELINK_COMPONENT libinputtino-dev
                ARCHIVE COMPONENT libinputtino-dev
                INCLUDES DESTINATION "${CMAKE_INSTALL_INCLUDEDIR}")
        install(DIRECTORY include/
                TYPE INCLUDE
                COMPONENT libinputtino-dev)
        install(FILES "${CMAKE_CURRENT_BINARY_DIR}/include/inputtino/${export_file_name}"
                COMPONENT libinputtino-dev
                DESTINATION "${CMAKE_INSTALL_INCLUDEDIR}/inputtino")
        configure_file(
                ${CMAKE_CURRENT_SOURCE_DIR}/share/pkgconfig/libinputtino.pc.in
                ${CMAKE_CURRENT_BINARY_DIR}/libinputtino.pc
                @ONLY
        )
        install(FILES ${CMAKE_CURRENT_BINARY_DIR}/libinputtino.pc
                DESTINATION ${CMAKE_INSTALL_DATAROOTDIR}/pkgconfig)
    endif ()
endif ()

target_include_directories(libinputtino PUBLIC
        "$<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/include>"
        "$<BUILD_INTERFACE:${CMAKE_CURRENT_BINARY_DIR}/include>")
set_target_properties(libinputtino PROPERTIES
        VERSION ${PROJECT_VERSION}
        PUBLIC_HEADER "${PUBLIC_HEADERS}")
set_target_properties(libinputtino PROPERTIES OUTPUT_NAME "libinputtino")

# Hardware-independent regression coverage for the locally added Edge report bits.
option(INPUTTINO_EDGE_TESTS "Test DualSense Edge input reports without UHID" OFF)
if(INPUTTINO_EDGE_TESTS)
    enable_testing()
    add_executable(dualsense-edge-test tests/dualsense_edge.cpp)
    target_include_directories(dualsense-edge-test PRIVATE src/uhid/include ${CMAKE_CURRENT_BINARY_DIR}/include)
    target_link_libraries(dualsense-edge-test PRIVATE inputtino::libinputtino)
    add_test(NAME dualsense-edge-report COMMAND dualsense-edge-test)
endif()
