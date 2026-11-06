// /share — upload a file or a folder to filedrop and show the share link.
//
// A folder is sent as one .zip built here in the browser. It is a "stored" zip
// (no compression): most of what gets shared this way — photographs, video,
// archives — is already compressed, and storing means the zip can be a Blob
// stitched together from the picked files themselves, so nothing the size of
// the upload is ever copied into memory. The one full read is computing each
// file's CRC-32, which the format requires before the file's data.
//
// A file or folder can also be dropped onto the drop zone; a dropped folder is
// walked with the File System entries API and zipped the same way.
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
  var drop = document.getElementById('share-drop');
  var chosen = document.getElementById('share-chosen');
  var chosenText = document.getElementById('share-chosen-text');
  var clearBtn = document.getElementById('share-clear');
  var submit = form.querySelector('button[type="submit"]');
  var expiryRadios = form.querySelectorAll('input[name="expires"]');
  var stepButtons = form.querySelectorAll('.share-step');
  var downloads = form.elements.downloads;
  var unlimited = form.elements.unlimited;
  var downloadsField = document.getElementById('share-downloads-field');
  var progressBox = document.getElementById('share-progress');
  var bar = progressBox.querySelector('progress');
  var statusEl = progressBox.querySelector('.share-status');
  var errorEl = document.getElementById('share-error');
  var resultBox = document.getElementById('share-result');
  var linkEl = document.getElementById('share-link');
  var copyBtn = document.getElementById('share-copy');
  var summaryEl = document.getElementById('share-summary');

  // { name, size, file } for a file; { name, size, files } for a folder, where
  // each of `files` is { file, path } and `path` is its name inside the zip.
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
    clearBtn.disabled = on;
    drop.classList.toggle('is-busy', on);
    for (var i = 0; i < expiryRadios.length; i++) expiryRadios[i].disabled = on;
    for (var j = 0; j < stepButtons.length; j++) stepButtons[j].disabled = on;
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

  function select(sel) {
    selection = sel;
    chosenText.textContent = describe(sel);
    chosen.hidden = !sel;
    drop.classList.toggle('has-file', !!sel);
    sync();
  }

  function folderSelection(name, entries) {
    if (!entries.length) return null;
    var size = 0;
    for (var i = 0; i < entries.length; i++) size += entries[i].file.size;
    return { name: name, size: size, files: entries };
  }

  fileInput.addEventListener('change', function () {
    var f = fileInput.files[0];
    folderInput.value = '';
    select(f ? { name: f.name, size: f.size, file: f } : null);
  });

  folderInput.addEventListener('change', function () {
    var files = Array.prototype.slice.call(folderInput.files);
    fileInput.value = '';
    var entries = files.map(function (f) {
      return { file: f, path: f.webkitRelativePath || f.name };
    });
    var root = entries.length ? entries[0].path.split('/')[0] : '';
    select(folderSelection(root, entries));
  });

  clearBtn.addEventListener('click', function () {
    fileInput.value = '';
    folderInput.value = '';
    select(null);
  });

  // ---- Drag and drop -----------------------------------------------------

  function readAllEntries(reader) {
    // readEntries hands back a directory in batches; an empty batch is the end.
    return new Promise(function (resolve, reject) {
      var out = [];
      (function next() {
        reader.readEntries(function (batch) {
          if (!batch.length) return resolve(out);
          out = out.concat(batch);
          next();
        }, reject);
      })();
    });
  }

  async function walk(entry, prefix, out) {
    if (entry.isFile) {
      var f = await new Promise(function (resolve, reject) {
        entry.file(resolve, reject);
      });
      out.push({ file: f, path: prefix + f.name });
    } else if (entry.isDirectory) {
      var kids = await readAllEntries(entry.createReader());
      for (var i = 0; i < kids.length; i++) {
        await walk(kids[i], prefix + entry.name + '/', out);
      }
    }
  }

  // Without this a file dropped a few pixels outside the zone would make the
  // browser navigate away to display it.
  window.addEventListener('dragover', function (ev) {
    ev.preventDefault();
  });
  window.addEventListener('drop', function (ev) {
    ev.preventDefault();
  });

  var dragDepth = 0;
  drop.addEventListener('dragenter', function (ev) {
    ev.preventDefault();
    if (busy) return;
    dragDepth++;
    drop.classList.add('is-over');
  });
  drop.addEventListener('dragleave', function () {
    dragDepth = Math.max(0, dragDepth - 1);
    if (!dragDepth) drop.classList.remove('is-over');
  });
  drop.addEventListener('drop', async function (ev) {
    ev.preventDefault();
    dragDepth = 0;
    drop.classList.remove('is-over');
    if (busy || !ev.dataTransfer) return;

    // Entries must be taken while the event is being dispatched; the list is
    // emptied once the handler yields.
    var items = ev.dataTransfer.items;
    var entries = [];
    if (items) {
      for (var i = 0; i < items.length; i++) {
        var e = items[i].webkitGetAsEntry && items[i].webkitGetAsEntry();
        if (e) entries.push(e);
      }
    }
    var plain = Array.prototype.slice.call(ev.dataTransfer.files || []);
    fileInput.value = '';
    folderInput.value = '';

    if (!entries.length) {
      // No entries API: plain files only.
      if (plain.length === 1) select({ name: plain[0].name, size: plain[0].size, file: plain[0] });
      else select(folderSelection('files', plain.map(function (f) {
        return { file: f, path: 'files/' + f.name };
      })));
      return;
    }
    if (entries.length === 1 && entries[0].isFile && plain.length === 1) {
      select({ name: plain[0].name, size: plain[0].size, file: plain[0] });
      return;
    }
    // One folder keeps its own name; several dropped things are gathered
    // under one top-level folder so the zip unpacks tidily.
    var root = entries.length === 1 ? entries[0].name : 'files';
    var prefix = entries.length === 1 ? '' : 'files/';
    var out = [];
    try {
      for (var j = 0; j < entries.length; j++) await walk(entries[j], prefix, out);
    } catch (err) {
      showError(form.dataset.errFailed);
      return;
    }
    select(folderSelection(root, out));
  });

  function checkedExpiry() {
    for (var i = 0; i < expiryRadios.length; i++) {
      if (expiryRadios[i].checked) return expiryRadios[i];
    }
    return expiryRadios[expiryRadios.length - 1];
  }

  // ---- Download count ----------------------------------------------------

  for (var s = 0; s < stepButtons.length; s++) {
    stepButtons[s].addEventListener('click', function () {
      var n = parseInt(downloads.value, 10) || 1;
      n += Number(this.dataset.step);
      n = Math.min(Number(downloads.max) || 1000, Math.max(Number(downloads.min) || 1, n));
      downloads.value = n;
    });
  }

  // An unlimited link has no limit to set, so the field goes away rather than
  // sitting there greyed out. Disabled as well as hidden, so the browser's
  // validation never stops the form over a field nobody can see.
  function syncUnlimited() {
    downloadsField.hidden = unlimited.checked;
    downloads.disabled = busy || unlimited.checked;
  }
  unlimited.addEventListener('change', syncUnlimited);

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
    for (var i = 0; i < files.length; i++) total += files[i].file.size;
    var done = 0;
    var parts = [];
    var central = [];
    var offset = 0;

    for (var j = 0; j < files.length; j++) {
      var f = files[j].file;
      var e = {
        name: encoder.encode(files[j].path),
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
      var q = 'name=' + encodeURIComponent(name) + '&expires=' + encodeURIComponent(checkedExpiry().value);
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
    return 'Expires in ' + checkedExpiry().parentNode.textContent.trim() + ' · ' + dl;
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
    if (resultBox.open) resultBox.close();
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
      resultBox.showModal();
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

  // Clicking the backdrop (the dialog element itself, outside its content box)
  // closes it, as a popup is expected to.
  resultBox.addEventListener('click', function (ev) {
    if (ev.target !== resultBox) return;
    var r = resultBox.getBoundingClientRect();
    var inside = ev.clientX >= r.left && ev.clientX <= r.right &&
      ev.clientY >= r.top && ev.clientY <= r.bottom;
    if (!inside) resultBox.close();
  });

  // A reload can restore the checkbox ticked; match the field to it.
  syncUnlimited();
  sync();
})();
