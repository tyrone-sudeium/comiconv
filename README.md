# Comiconv

Comic book converter written in rust.

## Features

- reads 7Z/CB7, TAR/CBT and ZIP/CBZ
- saves in the same archive format as input
- can convert images to JPEG, JPEGXL, PNG, WEBP and AVIF
- can convert locally or on a server running [comiconv-server](https://github.com/chrontax/comiconv-server)
- ships with `comiconv-web`, a web server that converts uploaded archives to AVIF over HTTP

## Web server

The `comiconv-web` binary runs a small web server that converts uploaded archives to AVIF on demand (max 1 GiB per upload, up to 4 conversions in parallel):

```sh
cargo run --bin comiconv-web            # listens on 127.0.0.1:8080
cargo run --bin comiconv-web 0.0.0.0:8080
```

- `GET /` — upload page with a progress bar
- `POST /convert` — raw binary body, e.g.

  ```sh
  curl --data-binary @comic.zip -o comic.cbz http://localhost:8080/convert
  ```

- `POST /upload` — multipart form data with a field named `file`, e.g.

  ```sh
  curl -F "file=@comic.zip" -o comic.cbz http://localhost:8080/upload
  ```

Both endpoints return the archive with every image re-encoded as AVIF, as an `application/x-cbz` attachment named after the uploaded file.

The AVIF quality can be chosen per request: `20` (Very Low), `33` (Low), `50` (High), `60` (Very High) or `auto` (the default). `auto` picks from the upload size: over 200&nbsp;MiB uses 33, over 50&nbsp;MiB uses 50, anything smaller uses 60. Pass it as a form field on `/upload` or as a `?quality=` query parameter on `/convert`:

```sh
curl -F "file=@comic.zip" -F "quality=50" -o comic.cbz http://localhost:8080/upload
curl --data-binary @comic.zip -o comic.cbz "http://localhost:8080/convert?quality=auto"
```

I would strongly recommend not hosting this web server on the public internet, as it is not security hardened and would be trivial to denial-of-service due to the intense resource usage.

## CLI Usage

```none
Usage: comiconv [OPTIONS] <FILES>...

Arguments:
  <FILES>...  Files to convert

Options:
  -s, --speed <VALUE>     Set speed: 0 (Slowest) - 10 (Fastest) (0-2 for png)
  -q, --quality <VALUE>   Set quality 0 (Worst) - 100 (Best) (ignored for webp, it's always lossless)
  -f, --format <VALUE>    Set format (avif, webp, jpeg, jxl, png)
  -w, --max-width <VALUE> Resizes (portrait only) images above this. Square/landscape images are never resized
      --quiet             Suppress progress messages
      --backup            Keep backup of original file
      --server <ADDRESS>  Server for online conversion
  -h, --help              Print help
  -V, --version           Print version
```

## Examples

Convert using default settings:

```sh
comiconv path/to/file
```

Convert to jpeg with quality 80:

```sh
comiconv -f jpeg -q 80 path/to/file
```
