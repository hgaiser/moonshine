{
  lib,
  stdenv,
  cmake,
  pkg-config,
  vulkan-loader,
}:

let
  # Authoritative Pyroshine source. Never replace this with Themaister's
  # upstream repository: the fork carries the color-metadata API used here.
  sourceUrl = "https://github.com/karsyboy/pyrowave";
  sourceRevision = "e344479d6c0439e346c788a918ad5645713f7573";
  graniteRevision = "1b2d1801d2910fb09ebcded2f0bb3a3a781103b5";
  volkRevision = "47cddf7ed97b94118a08aacb548a411188e016cc";
  vulkanHeadersRevision = "6802bb4733b63ed5efd3adb308a6c885ef180ea1";

  src = builtins.fetchGit {
    url = sourceUrl;
    rev = sourceRevision;
  };
  granite = builtins.fetchGit {
    url = "https://github.com/Themaister/Granite";
    rev = graniteRevision;
  };
  volk = builtins.fetchGit {
    url = "https://github.com/zeux/volk";
    rev = volkRevision;
  };
  vulkanHeaders = builtins.fetchGit {
    url = "https://github.com/KhronosGroup/Vulkan-Headers";
    rev = vulkanHeadersRevision;
  };
in
stdenv.mkDerivation {
  pname = "pyrowave-pyroshine";
  version = "0.7.0-${builtins.substring 0 8 sourceRevision}";
  inherit src;

  nativeBuildInputs = [
    cmake
    pkg-config
  ];
  buildInputs = [ vulkan-loader ];

  postUnpack = ''
    cp -r ${granite} $sourceRoot/Granite
    chmod -R u+w $sourceRoot/Granite
    mkdir -p $sourceRoot/Granite/third_party/khronos
    rm -rf $sourceRoot/Granite/third_party/volk
    rm -rf $sourceRoot/Granite/third_party/khronos/vulkan-headers
    cp -r ${volk} $sourceRoot/Granite/third_party/volk
    cp -r ${vulkanHeaders} $sourceRoot/Granite/third_party/khronos/vulkan-headers
  '';

  cmakeFlags = [
    "-DPYROWAVE_DEVEL=OFF"
    "-DPYROWAVE_UTILS=OFF"
    "-DBUILD_TESTING=OFF"
  ];

  # Runtime tests need a Vulkan GPU; the fork's CPU-only packet validation is
  # exercised by its own CI and Pyroshine checks its ABI at startup.
  doCheck = false;

  meta = {
    description = "Pinned PyroWave build for Pyroshine";
    homepage = sourceUrl;
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
  };
}
