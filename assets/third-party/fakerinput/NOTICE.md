# FakerInput 0.1.1 x64

This directory contains the unmodified official Windows Installer package,
embedded into the portable GameReady executable. It is installed only after
the user chooses the in-app installation action. Windows handles elevation
and driver approval. It is not installed on startup or silently with a fill.

- Source: https://github.com/Ryochan7/FakerInput (archived)
- Release: https://github.com/Ryochan7/FakerInput/releases/tag/v0.1.1
- Package: https://github.com/Ryochan7/FakerInput/releases/download/v0.1.1/FakerInput_Setup_0.1.1_x64.msi
- SHA-256: `4c0aefb7340051a91d606776243298b5cd1143ef5508bbae6800c474f9ed0840`
- Signed by: Ryodigi Solutions LLC
- License: MIT, copyright (c) 2021 Travis Nickles; see `LICENSE.txt`.
- License source: https://github.com/Ryochan7/FakerInput/blob/v0.1.1/LICENSE

The MSI hash and Authenticode signature are checked before release packaging
and before installation. A successful installer exit is followed by HID device
verification. GameReady never automatically restarts Windows or initiates login.
