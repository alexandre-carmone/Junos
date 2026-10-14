# INDIGO Astronomy (https://github.com/indigo-astronomy/indigo) — the
# indigo_server, its simulator drivers and the agents, built from the upstream
# tag. Only the drivers listed below are built: the full tree pulls in dozens of
# vendor camera SDKs that are useless on a dev box.
#
# Bump: change `tag`, set `hash = lib.fakeHash`, run `nix build .#indigo` and
# paste the hash it reports.
{ lib
, stdenv
, fetchFromGitHub
, pkg-config
, autoconf
, automake
, libtool
, util-linux
, libusb1
, avahi-compat
, zlib
, udev
}:

let
  # Subfolders of indigo_drivers/ to build. Each driver becomes a dlopen'ed
  # indigo_<name>.so in $out/lib; the ones with a *_main.c also get a
  # standalone indigo_<name> executable in $out/bin (speaks INDI over stdio).
  drivers = [
    "ccd_simulator"
    "mount_simulator"
    "dome_simulator"
    "gps_simulator"
    "rotator_simulator"
    "polaralign_simulator"
    "agent_imager"
    "agent_mount"
    "agent_guider"
    "agent_astrometry"
    "agent_alpaca"
  ];
in
stdenv.mkDerivation rec {
  pname = "indigo";
  version = "3.0-8";

  src = fetchFromGitHub {
    owner = "indigo-astronomy";
    repo = "indigo";
    tag = version;
    hash = "sha256-/y5VKb7HB11dmo98kiq/nbydDOxSUFSx/A9CEHIL59M=";
  };

  nativeBuildInputs = [
    pkg-config
    # the vendored hidapi runs autoreconf
    autoconf
    automake
    libtool
    # the server embeds its control panel through `gzip | hexdump`
    util-linux
  ];

  # libjpeg, libtiff, libraw and hidapi are vendored under indigo_libs/externals
  # and linked statically; these are what libindigo links dynamically.
  buildInputs = [
    libusb1
    avahi-compat # avahi-client/-common plus the dns_sd.h compat layer
    zlib
    udev
  ];

  postPatch = ''
    # The link flags carry $ORIGIN-relative rpaths (../lib, ../drivers) that
    # patchelf would strip; point every binary and library at $out/lib instead.
    # indigo_load_driver() dlopens a driver by bare file name from inside
    # libindigo.so, so that library's RUNPATH is the driver search path.
    sed -i 's|-Wl,-rpath=[^ ]*|-Wl,-rpath='"$out"'/lib|' Makefile

    # The libftd2xx vendor archive is unpacked through /tmp, absent from the sandbox.
    substituteInPlace indigo_libs/Makefile --replace-fail '/tmp' "$TMPDIR/ftd2xx"

    # Written for dash: under bash the "\n" would be copied into the generated C file.
    substituteInPlace tools/make_indigo_build.sh --replace-fail '\n' ""
  '';

  # The top-level `all` target also greps the tree for TODOs (and fails on a
  # clean grep) and builds every driver; drive the sub-makefiles directly, the
  # way Makefile.drvs does, for just the folders we want.
  buildPhase = ''
    runHook preBuild

    mkdir -p "$TMPDIR/ftd2xx"
    # indigo_driver_metadata dlopens each freshly built driver, which needs
    # libindigo.so from the build tree before anything is installed.
    export LD_LIBRARY_PATH="$PWD/build/lib"

    make init
    make -j"$NIX_BUILD_CORES" -C indigo_libs all
    make -j"$NIX_BUILD_CORES" -C indigo_tools all
    for d in ${lib.concatStringsSep " " drivers}; do
      make -j"$NIX_BUILD_CORES" -C "indigo_drivers/$d" -f ../../Makefile.drv all
      build/bin/indigo_driver_metadata "build/drivers/indigo_$d" >> build/share/indigo/indigo_drivers
    done
    make -j"$NIX_BUILD_CORES" -C indigo_server all

    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall

    mkdir -p "$out"/{bin,lib,include,etc,share/indigo,lib/udev/rules.d,lib/firmware}
    installVars=(
      INSTALL_ROOT="$out"
      INSTALL_BIN="$out/bin"
      INSTALL_LIB="$out/lib"
      INSTALL_INCLUDE="$out/include"
      INSTALL_ETC="$out/etc"
      INSTALL_SHARE="$out/share"
      INSTALL_RULES="$out/lib/udev/rules.d"
      INSTALL_FIRMWARE="$out/lib/firmware"
    )
    make -C indigo_libs install "''${installVars[@]}"
    make -C indigo_tools install "''${installVars[@]}"
    for d in ${lib.concatStringsSep " " drivers}; do
      make -C "indigo_drivers/$d" -f ../../Makefile.drv install "''${installVars[@]}"
    done
    make -C indigo_server install "''${installVars[@]}"
    # The driver list the server reads from <bin>/../share/indigo at startup.
    cp build/share/indigo/indigo_drivers "$out/share/indigo/"

    # A perl script with no perl around.
    rm -f "$out/bin/indigo_log_analyzer"
    find "$out" -type d -empty -delete

    runHook postInstall
  '';

  # Keep the $out/lib rpath on libindigo.so: it is not a DT_NEEDED provider for
  # that library, only the dlopen search path, so `patchelf --shrink-rpath`
  # would remove it and the server would then find no driver.
  dontPatchELF = true;

  meta = {
    description = "INDIGO Astronomy server, simulator drivers and agents";
    homepage = "https://www.indigo-astronomy.org";
    # "INDIGO Astronomy open-source license", a BSD-style permissive license.
    license = lib.licenses.free;
    platforms = lib.platforms.linux;
    mainProgram = "indigo_server";
  };
}
