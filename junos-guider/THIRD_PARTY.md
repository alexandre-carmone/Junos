# Third-party notices

`junos-guider` is a Rust port of the guiding engine, calibration, tools, logs
and event-server protocol of **PHD2 Guiding 2.6.14** (https://openphdguiding.org,
https://github.com/OpenPHDGuiding/phd2). PHD2 is distributed under the
BSD-3-Clause license reproduced below; every file ported from it opens with a
comment naming its PHD2 source file. The port itself is released under the
workspace license (GPL-3.0-or-later), which the BSD-3-Clause permits.

Copyright holders of the ported code:

- Copyright (c) 2013-2019, Open PHD Guiding development team
  (`src/*.cpp` — guide algorithms by Bret McKee and Craig Stark, star finding,
  calibration, backlash compensation, Guiding Assistant, polar tools, logs,
  event server, gear simulator).
- Copyright (c) 2014-2015, Max Planck Society — Edgar D. Klenske, Stephan
  Wenninger, Raffi Enficiaud (`contributions/MPI_IS_gaussian_process`: the
  Gaussian-process "Predictive PEC" guider, kernels, period estimation).
- Copyright (c) 2018 Ken Self (`src/guide_algorithm_zfilter.{h,cpp}`,
  `src/zfilterfactory.{h,cpp}`: the ZFilter guide algorithm and its design),
  the design based on *mkfilter* by A.J. Fisher, University of York,
  September 1992 (https://www-users.cs.york.ac.uk/~fisher/mkfilter/).
- Copyright (c) 2017 Ken Self (`src/polardrift_toolwin.cpp`,
  `src/staticpa_toolwin.cpp`: the polar drift and static polar alignment tools).

## BSD-3-Clause

```
Redistribution and use in source and binary forms, with or without modification,
are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice,
   this list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

3. Neither the name of the copyright holder nor the names of its contributors
   may be used to endorse or promote products derived from this software without
   specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED.
IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT,
INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING,
BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE
OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED
OF THE POSSIBILITY OF SUCH DAMAGE.
```
