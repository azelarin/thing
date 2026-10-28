# font fixtures

these files exercise the skia renderer's variable-font, collection, and
synthetic-bold paths. they are test data only.

all three files are byte-for-byte copies from
[google/skia at `8199836f8707b259afded3ee2904fea2bb93f432`](https://github.com/google/skia/tree/8199836f8707b259afded3ee2904fea2bb93f432).

| file | skia path | git blob | sha-256 | license |
| --- | --- | --- | --- | --- |
| `RobotoSlab-VariableFont_wght.ttf` | `modules/canvaskit/tests/assets/RobotoSlab-VariableFont_wght.ttf` | `b14200ebcb3a04e122be769b914babee9727b4f6` | `181c83564e9cd1e0426a573dedf3ed6d9191c710c268aeb458e18ec5ad1031fd` | [Apache-2.0](LICENSE-RobotoSlab-Apache-2.0.txt) |
| `Bungee-Regular.ttf` | `modules/canvaskit/tests/assets/Bungee-Regular.ttf` | `3229ee24d0ed60fd362b8115fe7450e445f7e33a` | `9072843fd22ee597afd6c5fa7d8e1202833315d34b7a2a40889d280a01cc53bc` | [OFL-1.1](LICENSE-Bungee-OFL-1.1.txt) |
| `NotoSansCJK-VF-subset.otf.ttc` | `resources/fonts/NotoSansCJK-VF-subset.otf.ttc` | `24b85d389c9627a49f17c9f87e0f4abb14558051` | `e7f71fc8aec139bb21cc541067eabb162b87aeeac0ccfcb3c835a20d0cee340a` | [OFL-1.1](LICENSE-NotoSansCJK-OFL-1.1.txt) |

the font metadata identifies these copyright holders:

- Roboto Slab: Copyright 2018 The Roboto Slab Project Authors.
- Bungee: Copyright 2008 The Bungee Project Authors.
- Noto Sans CJK: Copyright 2014-2023 Adobe. `Source` is a reserved font name.

the license files are pinned copies from the upstream font repositories:

- [Roboto Slab at google/fonts `726818c7`](https://github.com/google/fonts/blob/726818c7767f8d3eef97efc2d260979d406e5864/apache/robotoslab/LICENSE.txt)
- [Bungee at google/fonts `6177da92`](https://github.com/google/fonts/blob/6177da92bf853e779b0e46ec9f0dd301b2726641/ofl/bungee/OFL.txt)
- [Noto Sans CJK at notofonts/noto-cjk `f8d15753`](https://github.com/notofonts/noto-cjk/blob/f8d157532fbfaeda587e826d4cd5b21a49186f7c/Sans/LICENSE)

Skia introduced the Noto subset in
[`7646dffe5844e53998e69be236ba5099fe02b435`](https://github.com/google/skia/commit/7646dffe5844e53998e69be236ba5099fe02b435)
and records that it was generated with `drott/noto-cjk`'s `subsetvf.py`.
