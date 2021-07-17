mergeInto(LibraryManager.library, {
  loadFiles: function() {
    const imports = { wasi_snapshot_preview1: WASIPolyfill };
    let file = document.getElementById("input").files[0];
    let file_with_mime_type = file.slice(0, file.size, "application/wasm");
    let response = new Response(file_with_mime_type);
    wasi_instantiateStreaming(response, imports)
      .then(obj => {
        setInstance(obj.instance);
        try {
          obj.instance.exports._start();
        } catch (e) {
          if (e instanceof WASIExit) {
            handleWASIExit(e);
          } else {
          }
        }
      })
      .catch(error => {
        console.log("error! " + error);
      });
  }
});
