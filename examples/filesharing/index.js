import * as wasm from 'wasmtorrent'

function download_file(data, name) {
    const blob = new Blob([data], {
        type: "application/octet-stream"
    })

    const link = document.createElement('a');
    link.style.display = 'none';
    link.href = URL.createObjectURL(blob);
    link.download = name

    // It needs to be added to the DOM so it can be clicked
    document.body.appendChild(link);
    link.click();

    // To make this work on Firefox we need to wait
    // a little while before removing it.
    setTimeout(() => {
        URL.revokeObjectURL(link.href);
        link.parentNode.removeChild(link);
    }, 0);
}

export default async (root, context) => {

    // extract info hash from url
    const magnet = window.location.hash

    if (magnet) {
        const torrent = wasm.Torrent.from_magnet_link(magnet)
        const { faucet, sink } = torrent.connection_pool()
        context.socket.addEventListener("newPeer", async (ev) => {
            await faucet.send(ev.detail)
        })
        const metadata = await torrent.download_metadata(sink)
        console.info(metadata)
        const file = await wasm.Torrent.from_metadata(metadata).download_pieces(sink)
        download_file(file, "test")
        return
    }

    root.innerHTML = `
        <div id="drop-area" style="
            border: 2px dashed #ccc;
            border-radius: 20px;
            width: 480px;
            font-family: sans-serif;
            margin: 100px auto;
            padding: 20px;
        ">
        <form class="my-form">
            <p>Drag and drop files here to generate magnet link</p>
            <input type="file" id="file-selector">
        </form>
        </br>
        </div>
    `

    const dropArea = root.getElementById('drop-area')

    ;['dragenter', 'dragover', 'dragleave', 'drop'].forEach(eventName => {
        dropArea.addEventListener(eventName, preventDefaults, false)
    })

    function preventDefaults (e) {
        e.preventDefault()
        e.stopPropagation()
    }

    ;['dragenter', 'dragover'].forEach(eventName => {
        dropArea.addEventListener(eventName, () => { 
            dropArea.style.borderColor = "purple"
        }, false)
    })
    
    ;['dragleave', 'drop'].forEach(eventName => {
        dropArea.addEventListener(eventName, () => {
            dropArea.style.borderColor = "#ccc"
        }, false)
    })

    dropArea.addEventListener('drop', handleDrop, false)

    function handleDrop(e) {
        const files = e.dataTransfer.files
        handleFile(files[0])
    }

    async function handleFile(uploadFile) {
        let bytes = new Uint8Array(await uploadFile.arrayBuffer())
        const torrent = wasm.Torrent.from_file("test", bytes)
        const magnet = torrent.build_magnet_link()
        const url = root.getElementById("torrent-url")
        url.value = `${window.location.href}#${magnet}`

        const { faucet, sink } = torrent.connection_pool()
        context.socket.addEventListener("newPeer", async (ev) => await faucet.send(ev.detail))
        await torrent.seed_pieces(sink, bytes)
    }

    // url
    let url = document.createElement("div");
    url.innerHTML = `<input id="torrent-url" style="width: 400px;">`;
    root.getElementById("drop-area").append(url);

    root.getElementById("file-selector").addEventListener("change", async (e) => {
        let files = Array.from(e.target.files)
        await handleFile(files[0])
    });
}
