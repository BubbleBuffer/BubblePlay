# Pins the Windows SDK to 10.0.22621 for `flutter build windows`.
#
# On the owner's PC the 10.0.26100 SDK ships without the x64 gdi32.lib, so the default
# toolchain fails with MSB8037. Use it only where that SDK is installed:
#   $env:CMAKE_TOOLCHAIN_FILE = "<repo>\app\windows\toolchain-sdk22621.cmake"
# CMake reads that variable for the first configure, so run `flutter clean` after changing it.
# It does nothing for non-Visual Studio generators. See docs/app/README.md ("Building on Windows").
if(CMAKE_GENERATOR MATCHES "Visual Studio")
  set(CMAKE_GENERATOR_PLATFORM "x64,version=10.0.22621.0")
endif()
