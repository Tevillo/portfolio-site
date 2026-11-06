// /share — upload a file or a folder to filedrop and show the share link.
//
// A folder is sent as one .zip built here in the browser. It is a "stored" zip
// (no compression): most of what gets shared this way — photographs, video,
// archives — is already compressed, and storing means the zip can be a Blob
// stitched together from the picked files themselves, so nothing the size of
// the upload is ever copied into memory. The one full read is computing each
// file's CRC-32, which the format requires before the file's data.
//
// The upload goes to /share/upload, which streams it on to filedrop with the
// site's API key; XHR rather than fetch because only XHR reports upload
// progress.
(function () {
  var form = document.getElementById('share-form');
  if (!form) return;

  var MAX = Number(form.dataset.maxBytes);
  // Classic zip without Zip64 extensions: 16-bit entry count. The 32-bit
  // sizes and offsets are safe, since MAX is under 4 GiB.
  var MAX_ENTRIES = 65535;

  var fileInput = document.getElementById('share-file');
  var folderInput = document.getElementById('share-folder');
  var chosen = document.getElementById('share-chosen');
  var submit = form.querySelector('button[type="submit"]');
  var expires = form.elements.expires;
  var downloads = form.elements.downloads;
  var unlimited = form.elements.unlimited;
  var progressBox = document.getElementById('share-progress');
  var bar = progressBox.querySelector('progress');
  var statusEl = progressBox.querySelector('.share-status');
  var errorEl = document.getElementById('share-error');
  var resultBox = document.getElementById('share-result');
  var linkEl = document.getElementById('share-link');
  var copyBtn = document.getElementById('share-copy');
  var summaryEl = document.getElementById('share-summary');

  // { name, size, file } for a file, { name, size, files } for a folder.
  var selection = null;
  var busy = false;

  function fmtBytes(n) {
    var units = ['bytes', 'KB', 'MB', 'GB'];
    var i = 0;
    while (n >= 1024 && i < units.length - 1) {
      n /= 1024;
      i++;
    }
    return (i === 0 ? n : n.toFixed(n < 10 ? 1 : 0)) + ' ' + units[i];
  }

  function showError(msg) {
    errorEl.textContent = msg;
    errorEl.hidden = !msg;
  }

  function setBusy(on) {
    busy = on;
    fileInput.disabled = on;
    folderInput.disabled = on;
    expires.disabled = on;
    unlimited.disabled = on;
    downloads.disabled = on || unlimited.checked;
    sync();
  }

  function sync() {
    var tooBig = selection && selection.size > MAX;
    var tooMany = selection && selection.files && selection.files.length > MAX_ENTRIES;
    submit.disabled = busy || !selection || tooBig || tooMany;
    if (!busy) {
      showError(tooBig ? form.dataset.errTooLarge : tooMany ? form.dataset.errTooMany : '');
    }
  }

  function describe(sel) {
    if (!sel) return '';
    var parts = [sel.name, fmtBytes(sel.size)];
    if (sel.files) parts.push(sel.files.length + (sel.files.length === 1 ? ' file' : ' files'));
    return parts.join(' · ');
  }

  fileInput.addEventListener('change', function () {
    var f = fileInput.files[0];
    folderInput.value = '';
    selection = f ? { name: f.name, size: f.size, file: f } : null;
    chosen.textContent = describe(selection);
    sync();
  });

  folderInput.addEventListener('change', function () {
    var files = Array.prototype.slice.call(folderInput.files);
    fileInput.value = '';
    if (!files.length) {
      selection = null;
    } else {
      var root = (files[0].webkitRelativePath || files[0].name).split('/')[0];
      var size = 0;
      for (var i = 0; i < files.length; i++) size += files[i].size;
      selection = { name: root, size: size, files: files };
    }
    chosen.textContent = describe(selection);
    sync();
  });

  unlimited.addEventListener('change', function () {
    downloads.disabled = busy || unlimited.checked;
  });

  // ---- Stored zip --------------------------------------------------------

  var CRC_TABLE = (function () {
    var t = new Uint32Array(256);
    for (var n = 0; n < 256; n++) {
      var c = n;
      for (var k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
      t[n] = c >>> 0;
    }
    return t;
  })();

  async function crc32(file, onBytes) {
    var crc = 0xffffffff;
    var reader = file.stream().getReader();
    for (;;) {
      var r = await reader.read();
      if (r.done) break;
      var buf = r.value;
      for (var i = 0; i < buf.length; i++) {
        crc = CRC_TABLE[(crc ^ buf[i]) & 0xff] ^ (crc >>> 8);
      }
      onBytes(buf.length);
    }
    return (crc ^ 0xffffffff) >>> 0;
  }

  function dosDateTime(ms) {
    var d = new Date(ms || Date.now());
    var year = Math.max(d.getFullYear(), 1980);
    return {
      time: (d.getHours() << 11) | (d.getMinutes() << 5) | (d.getSeconds() >> 1),
      date: ((year - 1980) << 9) | ((d.getMonth() + 1) << 5) | d.getDate(),
    };
  }

  var encoder = new TextEncoder();

  // Fields shared by the local header (from offset 4) and the central
  // directory entry (from offset 6): version needed, flags, method, time,
  // date, crc, sizes, name length.
  function common(view, at, e) {
    view.setUint16(at, 20, true); // version needed: 2.0
    view.setUint16(at + 2, 0x0800, true); // flags: names are UTF-8
    view.setUint16(at + 4, 0, true); // method: stored
    view.setUint16(at + 6, e.dt.time, true);
    view.setUint16(at + 8, e.dt.date, true);
    view.setUint32(at + 10, e.crc, true);
    view.setUint32(at + 14, e.size, true); // compressed = uncompressed
    view.setUint32(at + 18, e.size, true);
    view.setUint16(at + 22, e.name.length, true);
    view.setUint16(at + 24, 0, true); // extra field length
  }

  async function buildZip(files, onProgress) {
    var total = 0;
    for (var i = 0; i < files.length; i++) total += files[i].size;
    var done = 0;
    var parts = [];
    var central = [];
    var offset = 0;

    for (var j = 0; j < files.length; j++) {
      var f = files[j];
      var e = {
        name: encoder.encode(f.webkitRelativePath || f.name),
        size: f.size,
        dt: dosDateTime(f.lastModified),
        crc: await crc32(f, function (n) {
          done += n;
          onProgress(total ? done / total : 1);
        }),
        offset: offset,
      };

      var local = new Uint8Array(30 + e.name.length);
      var lv = new DataView(local.buffer);
      lv.setUint32(0, 0x04034b50, true);
      common(lv, 4, e);
      local.set(e.name, 30);
      parts.push(local, f);
      offset += local.length + f.size;

      var cd = new Uint8Array(46 + e.name.length);
      var cv = new DataView(cd.buffer);
      cv.setUint32(0, 0x02014b50, true);
      cv.setUint16(4, 20, true); // version made by
      common(cv, 6, e);
      // comment length, disk number, internal and external attributes: 0
      cv.setUint32(42, e.offset, true);
      cd.set(e.name, 46);
      central.push(cd);
    }

    var cdSize = 0;
    for (var k = 0; k < central.length; k++) cdSize += central[k].length;
    var end = new Uint8Array(22);
    var ev = new DataView(end.buffer);
    ev.setUint32(0, 0x06054b50, true);
    ev.setUint16(8, files.length, true);
    ev.setUint16(10, files.length, true);
    ev.setUint32(12, cdSize, true);
    ev.setUint32(16, offset, true);

    return new Blob(parts.concat(central, [end]), { type: 'application/zip' });
  }

  // ---- Upload ------------------------------------------------------------

  function upload(body, name, contentType) {
    return new Promise(function (resolve, reject) {
      var q = 'name=' + encodeURIComponent(name) + '&expires=' + encodeURIComponent(expires.value);
      q += '&downloads=' + (unlimited.checked ? '-1' : encodeURIComponent(downloads.value));
      var xhr = new XMLHttpRequest();
      xhr.open('PUT', '/share/upload?' + q);
      if (contentType) xhr.setRequestHeader('Content-Type', contentType);
      xhr.upload.onprogress = function (ev) {
        if (ev.lengthComputable) {
          bar.value = ev.loaded / ev.total;
          statusEl.textContent = 'Uploading… ' + Math.floor((ev.loaded / ev.total) * 100) + '%';
        }
      };
      xhr.onload = function () {
        var text = (xhr.responseText || '').trim();
        if (xhr.status === 201) resolve(text);
        else reject(new Error(text || form.dataset.errFailed));
      };
      xhr.onerror = function () {
        reject(new Error(form.dataset.errFailed));
      };
      xhr.send(body);
    });
  }

  function summary() {
    var n = Number(downloads.value);
    var dl = unlimited.checked ? 'unlimited downloads' : n + (n === 1 ? ' download' : ' downloads');
    return 'Expires in ' + expires.options[expires.selectedIndex].text + ' · ' + dl;
  }

  form.addEventListener('submit', async function (ev) {
    ev.preventDefault();
    if (!selection || busy) return;
    if (!unlimited.checked && !downloads.checkValidity()) {
      downloads.reportValidity();
      return;
    }
    var sel = selection;
    showError('');
    resultBox.hidden = true;
    setBusy(true);
    progressBox.hidden = false;
    bar.value = 0;
    try {
      var body, name, type;
      if (sel.files) {
        statusEl.textContent = 'Zipping folder…';
        body = await buildZip(sel.files, function (p) {
          bar.value = p;
          statusEl.textContent = 'Zipping folder… ' + Math.floor(p * 100) + '%';
        });
        name = sel.name + '.zip';
        type = 'application/zip';
        if (body.size > MAX) throw new Error(form.dataset.errTooLarge);
      } else {
        body = sel.file;
        name = sel.name;
        type = sel.file.type;
      }
      bar.value = 0;
      statusEl.textContent = 'Uploading… 0%';
      var link = await upload(body, name, type);
      linkEl.value = link;
      summaryEl.textContent = summary();
      resultBox.hidden = false;
      linkEl.focus();
      linkEl.select();
    } catch (err) {
      showError(err.message || form.dataset.errFailed);
    } finally {
      progressBox.hidden = true;
      setBusy(false);
    }
  });

  copyBtn.addEventListener('click', function () {
    var done = function () {
      copyBtn.textContent = 'Copied';
      setTimeout(function () {
        copyBtn.textContent = 'Copy';
      }, 1500);
    };
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(linkEl.value).then(done, function () {
        linkEl.select();
      });
    } else {
      linkEl.select();
      document.execCommand('copy');
      done();
    }
  });

  sync();
})();
