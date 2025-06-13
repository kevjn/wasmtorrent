import React from "react"
import { createRoot } from 'react-dom/client'
import * as wasm from "wasmtorrent"

const App = ({socket}) => {
  const magnet = window.location.hash
  if (!magnet) return (
    <p>Invalid magnet link</p>
  )

  const torrentRef = React.useRef(null)
  const sinkRef = React.useRef(null)
  const videoRef = React.useRef(null)

  const [name, setName] = React.useState(null)

  const setup = async () => {
    const torrent = wasm.Torrent.from_magnet_link(magnet)
    const { faucet, sink } = torrent.connection_pool()
    socket.addEventListener("newPeer", async ({detail}) => {
      await faucet.send(detail)
    })

    const metadata = await torrent.download_metadata(sink)
    torrentRef.current = wasm.Torrent.from_metadata(metadata)
    sinkRef.current = sink
    setName(torrentRef.current.name)
  }

  // need to be specific with codec
  const mimeCodec = 'video/mp4; codecs="avc1.42E01E, mp4a.40.2"';

  async function sourceOpen() {
    const sourceBuffer = this.addSourceBuffer(mimeCodec)
    sourceBuffer.addEventListener("updateend", () => {
      this.endOfStream()
      videoRef.current.play()
    })
    await torrentRef.current.download_pieces_to_source_buf(sinkRef.current, sourceBuffer)
  }

  const loadVideo = () => {
    if (MediaSource.isTypeSupported(mimeCodec)) {
      const mediaSource = new MediaSource()
      videoRef.current.src = URL.createObjectURL(mediaSource)
      mediaSource.addEventListener("sourceopen", sourceOpen, { once: true })
    } else {
      console.error("Unsupported MIME type or codec: ", mimeCodec)
    }
  }

  React.useEffect(() => {
    setup()
  }, [])

  return (
    <div style={{display: "flex", margin: "5vw", border: "1px solid red", gap: "20px", flexDirection: "column"}}>
      <h4>{name}</h4>
      <video width="750" height="500" ref={videoRef} controls />
      <button onClick={loadVideo} >Load Video</button>
    </div>
  )
}

export default async (root, context) => {
  createRoot(root).render(<App socket={context.socket} />)
}
