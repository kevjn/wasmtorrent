# wasmtorrent

A BitTorrent client inteded for seamless cross-compatibility between web and native platforms. The Rust library can be built with `wasm-pack` to output JavaScript which can be used in the browser with WebRTC datachannels.

## Usage

Build the library with node targeted
```
wasm-pack build -t nodejs
```
this will create a `/pkg` folder in the root of the repository which can be imported in a node application.

Start a docker instance running Transmission (A popular BitTorrent client)
```
docker run --rm --name=transmission -e PUID=1000 -p 9091:9091 -p 51413:51413 linuxserver/transmission:latest
```
We're listening to port `51413` for incomming connections to Transmission.

Next, create a random 1MB testfile in the container and set it up for seeding. Take note of the infohash.
```
docker exec -u 1000 transmission sh -c "head -c 1M < /dev/urandom > downloads/complete/testfile"
docker exec -u 1000 transmission transmission-create -o "/watch/testfile.torrent" "downloads/complete/testfile"
docker exec transmission transmission-show /watch/testfile.torrent.added
```

Run the following with `node` to download the random tesftile which is seeded by Transmission
```javascript
const wasmtorrent = require("./pkg")
const net = require("net");

(async () => {
    // glue code to communicate with Transmission using TCP
    const client = new net.Socket()
    client.connect(51413, "0.0.0.0")

    const target = new EventTarget()
    // receive data on the channel
    client.on("data", async (data) => {
      const message = new Uint8Array(data).buffer
      target.dispatchEvent(new MessageEvent("message", { data: message }))
    })
    // send data on the channel
    target.addEventListener("sendMessage", event => {
      client.write(event.data)
    })

    // create torrent from magnet link and download metadata with our contrived socket
    let torrent = wasmtorrent.Torrent.from_magnet_link("magnet:?xt=urn:btih:<info-hash>") // FIXME: Replace <info-hash> with the "testfile" info hash

    // setup p2p connection and send the contrived socket `target` as a potential peer
    const { faucet, sink } = torrent.connection_pool()
    await faucet.send(target)

    // download metadata with contrived socket
    const metadata = await torrent.download_metadata(sink)

    torrent = wasmtorrent.Torrent.from_metadata(metadata)
    // finally, download the file and log the random bytes from file to the console
    const file = await torrent.download_pieces(sink)
    console.info(file)
})()
```
If the handshake was successful it should start downloading the torrent and indiciate with a progress bar.