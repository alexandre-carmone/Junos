# astrometry.net (https://github.com/dstndstn/astrometry.net) — the C solver
# tools only: solve-field, astrometry-engine, image2xy, fits-guess-scale and
# the fits-*/wcs-* utilities. INDIGO's Astrometry Agent shells out to
# `image2xy`, `solve-field` and `curl`, feeds the solver an xylist (never an
# image) and writes its own astrometry.cfg, so neither the Python layer
# (`make py`: swig, numpy) nor the plotting extras (`make extra`: cairo,
# netpbm) are built. Not in nixpkgs.
#
# Bump: change `version`, set `hash = lib.fakeHash`, run
# `nix build .#astrometry` and paste the hash it reports.
{ lib
, stdenv
, fetchurl
, pkg-config
, python3
, bzip2
, cfitsio
, gsl
, wcslib
, zlib
}:

stdenv.mkDerivation rec {
  pname = "astrometry";
  version = "0.97";

  src = fetchurl {
    url = "https://github.com/dstndstn/astrometry.net/releases/download/${version}/astrometry.net-${version}.tar.gz";
    hash = "sha256-5O7xtli6WtRiKCtmHAyjpcU4uhcW6FP3lwt7n6SjNFk=";
  };

  nativeBuildInputs = [
    pkg-config
    # solver/Makefile's install target renders etc/astrometry.cfg through
    # `python3 -c`; python is not needed at run time.
    python3
  ];

  # Every library is found through pkg-config (util/makefile.*). gsl ≥ 1.14 on
  # the system skips the bundled gsl-an; wcslib is optional (fit-wcs, wcs-*);
  # catalogs/ (a `solver` prerequisite) reads bzip2-compressed UCAC5 files.
  buildInputs = [ bzip2 cfitsio gsl wcslib zlib ];

  # The root `configure` is a stub that prints "Ok".
  dontConfigure = true;

  makeFlags = [
    "INSTALL_DIR=${placeholder "out"}"
    "SYSTEM_GSL=yes"
    # Defined-but-empty disables the -march=native probe (util/makefile.common).
    "ARCH_FLAGS="
  ];

  # The default `all` also builds plot/ (cairo); `solver` is the top-level
  # target that stops at util, catalogs, libkd, qfits-an and solver.
  buildFlags = [ "solver" ];
  enableParallelBuilding = false;

  # The top-level install-core also wants report.txt, demo/, man/ and the
  # python-only sdss/; drive the same sub-makefiles it does for the C parts.
  installPhase = ''
    runHook preInstall

    make -C util install-core $makeFlags
    make -C catalogs install $makeFlags
    make -C libkd install $makeFlags
    make -C qfits-an install $makeFlags
    make -C solver install $makeFlags

    # The python helper scripts (image2pnm, removelines, uniformize, …) need
    # numpy/astropy and only serve image inputs; the agent feeds xylists.
    # Drop them and the python package rather than ship broken scripts.
    rm -rf "$out/lib/python"
    grep -l '^#!.*python' "$out"/bin/* | xargs -r rm -f

    runHook postInstall
  '';

  meta = {
    description = "Astrometry.net blind plate solver (solve-field, image2xy)";
    homepage = "https://astrometry.net";
    license = lib.licenses.bsd3;
    platforms = lib.platforms.linux;
    mainProgram = "solve-field";
  };
}
