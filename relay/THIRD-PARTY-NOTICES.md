# Third-party notices for the Go relay

This notice covers the relay's pinned Go modules, the Go 1.27.1 runtime and standard library, and the standard library's vendored module copies. Original copyright attributions, license terms and patent grants are reproduced below. Some standard-library vendor entries may be broader than the packages linked into a particular target build.

The Docker build uses `golang:1.27.1-bookworm` with `CGO_ENABLED=0`; the runtime base is `scratch`. The runtime also carries a Debian CA certificate bundle. Its original copyright notice is retained at `/licenses/ca-certificates/copyright`, with referenced common-license texts under `/licenses/common-licenses/`. Those files accompany the bundle separately from these Go notices.

## Component inventory

| Component | Original retained files |
| --- | --- |
| Go standard runtime and library, go1.27.1 | [LICENSE](#license-text-2); [PATENTS](#license-text-3) |
| github.com/gorilla/websocket v1.5.3 | [LICENSE](#license-text-1) |
| golang.org/x/crypto v0.57.0 | [LICENSE](#license-text-2); [PATENTS](#license-text-3) |
| golang.org/x/term v0.46.0 | [LICENSE](#license-text-2); [PATENTS](#license-text-3) |
| golang.org/x/sys v0.48.0 | [LICENSE](#license-text-2); [PATENTS](#license-text-3) |
| golang.org/x/crypto v0.52.1-0.20260526024921-9beb694f9766 (vendored in go1.27.1) | [LICENSE](#license-text-2); [PATENTS](#license-text-3) |
| golang.org/x/net v0.55.1-0.20260731170536-c1d18010be90 (vendored in go1.27.1) | [LICENSE](#license-text-2); [PATENTS](#license-text-3) |
| golang.org/x/sys v0.45.0 (vendored in go1.27.1) | [LICENSE](#license-text-2); [PATENTS](#license-text-3) |
| golang.org/x/text v0.37.0 (vendored in go1.27.1) | [LICENSE](#license-text-2); [PATENTS](#license-text-3) |

The module versions come from the relay's `go.mod`/`go.sum`. Standard-library vendor versions come from the Go 1.27.1 distribution's `src/vendor/modules.txt`. The source license bytes were read from those exact cached module versions and the Go distribution. Identical texts share an appendix entry.

## Original license and patent texts

<a id="license-text-1"></a>

### License text 1

Original file SHA-256: `2be1b548b0387ca8948e1bb9434e709126904d15f622cc2d0d8e7f186e4d122d`.

Sources:

- github.com/gorilla/websocket v1.5.3/LICENSE

```text
Copyright (c) 2013 The Gorilla WebSocket Authors. All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

  Redistributions of source code must retain the above copyright notice, this
  list of conditions and the following disclaimer.

  Redistributions in binary form must reproduce the above copyright notice,
  this list of conditions and the following disclaimer in the documentation
  and/or other materials provided with the distribution.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

<a id="license-text-2"></a>

### License text 2

Original file SHA-256: `911f8f5782931320f5b8d1160a76365b83aea6447ee6c04fa6d5591467db9dad`.

Sources:

- Go standard runtime and library, go1.27.1/LICENSE
- golang.org/x/crypto v0.57.0/LICENSE
- golang.org/x/crypto v0.52.1-0.20260526024921-9beb694f9766 (vendored in go1.27.1)/LICENSE
- golang.org/x/net v0.55.1-0.20260731170536-c1d18010be90 (vendored in go1.27.1)/LICENSE
- golang.org/x/sys v0.48.0/LICENSE
- golang.org/x/sys v0.45.0 (vendored in go1.27.1)/LICENSE
- golang.org/x/term v0.46.0/LICENSE
- golang.org/x/text v0.37.0 (vendored in go1.27.1)/LICENSE

```text
Copyright 2009 The Go Authors.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are
met:

   * Redistributions of source code must retain the above copyright
notice, this list of conditions and the following disclaimer.
   * Redistributions in binary form must reproduce the above
copyright notice, this list of conditions and the following disclaimer
in the documentation and/or other materials provided with the
distribution.
   * Neither the name of Google LLC nor the names of its
contributors may be used to endorse or promote products derived from
this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
"AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
OWNER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

<a id="license-text-3"></a>

### License text 3

Original file SHA-256: `96f408bfae65bf137fc2525d3ecb030271c50c1e90799f87abf8846d8dd505cc`.

Sources:

- Go standard runtime and library, go1.27.1/PATENTS
- golang.org/x/crypto v0.57.0/PATENTS
- golang.org/x/crypto v0.52.1-0.20260526024921-9beb694f9766 (vendored in go1.27.1)/PATENTS
- golang.org/x/net v0.55.1-0.20260731170536-c1d18010be90 (vendored in go1.27.1)/PATENTS
- golang.org/x/sys v0.48.0/PATENTS
- golang.org/x/sys v0.45.0 (vendored in go1.27.1)/PATENTS
- golang.org/x/term v0.46.0/PATENTS
- golang.org/x/text v0.37.0 (vendored in go1.27.1)/PATENTS

```text
Additional IP Rights Grant (Patents)

"This implementation" means the copyrightable works distributed by
Google as part of the Go project.

Google hereby grants to You a perpetual, worldwide, non-exclusive,
no-charge, royalty-free, irrevocable (except as stated in this section)
patent license to make, have made, use, offer to sell, sell, import,
transfer and otherwise run, modify and propagate the contents of this
implementation of Go, where such license applies only to those patent
claims, both currently owned or controlled by Google and acquired in
the future, licensable by Google that are necessarily infringed by this
implementation of Go.  This grant does not include claims that would be
infringed only as a consequence of further modification of this
implementation.  If you or your agent or exclusive licensee institute or
order or agree to the institution of patent litigation against any
entity (including a cross-claim or counterclaim in a lawsuit) alleging
that this implementation of Go or any code incorporated within this
implementation of Go constitutes direct or contributory patent
infringement, or inducement of patent infringement, then any patent
rights granted to you under this License for this implementation of Go
shall terminate as of the date such litigation is filed.
```

