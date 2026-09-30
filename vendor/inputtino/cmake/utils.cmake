# set_if_undefined(<variable> [<value>]...)
#
# Set variable if it is not defined.
# Taken from: https://github.com/pananton/cpp-lib-template/
macro(set_if_undefined variable)
    if(NOT DEFINED "${variable}")
        set("${variable}" ${ARGN})
    endif()
endmacro()
