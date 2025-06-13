## Description
Incrementally downloads a mp4 video file and plays it using wasmtorrent.

## Usage
Make sure you have a mp4 file compatible with the codec. We will use frag_bunny video from MDN Media Source API: https://developer.mozilla.org/en-US/docs/Web/API/SourceBuffer#loading_a_video_chunk_by_chunk
```bash
wget https://github.com/nickdesaulniers/netfix/raw/refs/heads/gh-pages/demo/frag_bunny.mp4
ffprobe -v error -select_streams v:0 -show_entries stream=codec_tag_string,profile,level -of default=noprint_wrappers=1 frag_bunny.mp4
```
this should output
```
profile=Constrained Baseline
codec_tag_string=avc1
level=30
```
which translates to the codec `avc1.42E01E` and is compatible with all modern browsers. This file needs to be seeded.

The command `npm run dev` will only serve the bundled javascript with wasmtorrent. We need to import that url in an external web app and call its default function with provided parameters for shadow root and socket connection to the application seeding the file.
