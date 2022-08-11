export function download_file(data, name) {
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

    // const link = document.createElement('a');
    // link.href = window.URL.createObjectURL(blob);
    // link.download = 'File Name';
    // link.click();
}