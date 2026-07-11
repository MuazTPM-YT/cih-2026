/* =====================================================
   TGW Field Client — app.js
   Matches new DOM structure and UI mockup
   ===================================================== */

const $form         = document.getElementById("capture-form");
const $patientId    = document.getElementById("patient-id");
const $vitalTemp    = document.getElementById("vital-temp");
const $tempSite     = document.getElementById("temp-site");
const $siteReq      = document.getElementById("site-req");
const $vitalPulse   = document.getElementById("vital-pulse");
const $vitalResp    = document.getElementById("vital-resp");
const $bpSys        = document.getElementById("vital-bp-sys");
const $bpDia        = document.getElementById("vital-bp-dia");
const $notes        = document.getElementById("notes");
const $inputPhoto   = document.getElementById("input-take-photo");
const $inputUpload  = document.getElementById("input-upload-image");
const $imagePill    = document.getElementById("image-pill");
const $imageName    = document.getElementById("image-filename");
const $btnRemove    = document.getElementById("btn-remove-image");
const $btnSend      = document.getElementById("btn-send");
const $sentList     = document.getElementById("sent-list");
const $sentEmpty    = document.getElementById("sent-empty");

let selectedImageFile = null;
const sentItems = [];

function init() {
  $form.addEventListener("submit", handleSubmit);

  const validationInputs = [
    $patientId, $vitalTemp, $vitalPulse, $vitalResp,
    $bpSys, $bpDia, $notes, $tempSite
  ];
  validationInputs.forEach(el => {
    el.addEventListener("input", updateSendButton);
  });

  $vitalTemp.addEventListener("input", updateTempSiteRequired);

  if ($inputPhoto) $inputPhoto.addEventListener("change", handleImageSelect);
  if ($inputUpload) $inputUpload.addEventListener("change", handleImageSelect);
  $btnRemove.addEventListener("click", removeImage);

  // New Camera Button
  const $btnTakePhoto = document.getElementById("btn-take-photo");
  if ($btnTakePhoto) {
    $btnTakePhoto.addEventListener("click", openCamera);
  }

  // Camera Overlay Controls
  const $btnCameraClose = document.getElementById("btn-camera-close");
  const $btnCameraCapture = document.getElementById("btn-camera-capture");
  const $btnCameraRetry = document.getElementById("btn-camera-retry");
  if ($btnCameraClose) $btnCameraClose.addEventListener("click", closeCamera);
  if ($btnCameraCapture) $btnCameraCapture.addEventListener("click", capturePhoto);
  if ($btnCameraRetry) $btnCameraRetry.addEventListener("click", openCamera);

  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape") closeCamera();
  });
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) closeCamera();
  });

  // Keyboard support for file inputs
  document.querySelectorAll(".img-btn").forEach(label => {
    label.addEventListener("keydown", (e) => {
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        const input = label.querySelector("input[type=file]");
        if (input) input.click();
        else if (label.id === "btn-take-photo") openCamera();
      }
    });
  });

  updateSendButton();
  updateTempSiteRequired();
}

// --- Camera Logic ---
let currentStream = null;

async function openCamera() {
  const $cameraOverlay = document.getElementById("camera-overlay");
  const $cameraVideo = document.getElementById("camera-video");

  // Reset any previous error state and show the viewfinder chrome.
  hideCameraError();
  closeCamera();

  // getUserMedia needs a secure context (HTTPS or localhost) and a camera.
  // Without it we surface a clear error instead of silently opening a file picker.
  if (!navigator.mediaDevices?.getUserMedia) {
    showCameraError(
      !window.isSecureContext
        ? "Camera requires a secure page (HTTPS or localhost). Open this page via http://localhost to use the camera."
        : "This browser does not support camera access."
    );
    return;
  }

  try {
    const stream = await navigator.mediaDevices.getUserMedia({
      video: { facingMode: { ideal: "environment" } },
      audio: false
    });
    currentStream = stream;
    $cameraVideo.srcObject = stream;
    $cameraOverlay.hidden = false;

    await $cameraVideo.play();
  } catch (err) {
    console.error("Camera access denied or unavailable", err);
    closeCamera();
    showCameraError(cameraErrorMessage(err));
  }
}

function cameraErrorMessage(err) {
  const name = err && err.name ? err.name : "";
  switch (name) {
    case "NotAllowedError":
    case "SecurityError":
      return "Camera permission denied. Allow camera access in your browser settings and try again.";
    case "NotFoundError":
    case "DevicesNotFoundError":
      return "No camera found on this device.";
    case "NotReadableError":
    case "TrackStartError":
      return "The camera is in use by another app. Close it and try again.";
    case "OverconstrainedError":
      return "No camera matched the requested settings.";
    default:
      return "Could not start the camera. Please check permissions and try again.";
  }
}

function showCameraError(message) {
  const $cameraOverlay = document.getElementById("camera-overlay");
  const $cameraVideo = document.getElementById("camera-video");
  const $cameraControls = document.querySelector(".camera-controls");
  const $cameraError = document.getElementById("camera-error");
  const $cameraErrorText = document.getElementById("camera-error-text");

  $cameraVideo.hidden = true;
  if ($cameraControls) $cameraControls.style.display = "none";
  if ($cameraErrorText) $cameraErrorText.textContent = message;
  if ($cameraError) $cameraError.hidden = false;
  $cameraOverlay.hidden = false;
}

function hideCameraError() {
  const $cameraVideo = document.getElementById("camera-video");
  const $cameraControls = document.querySelector(".camera-controls");
  const $cameraError = document.getElementById("camera-error");

  $cameraVideo.hidden = false;
  if ($cameraControls) $cameraControls.style.display = "";
  if ($cameraError) $cameraError.hidden = true;
}

function closeCamera() {
  const $cameraOverlay = document.getElementById("camera-overlay");
  const $cameraVideo = document.getElementById("camera-video");

  if (currentStream) {
    currentStream.getTracks().forEach(track => track.stop());
    currentStream = null;
  }
  $cameraVideo.srcObject = null;
  hideCameraError();
  $cameraOverlay.hidden = true;
}

function capturePhoto() {
  const $cameraVideo = document.getElementById("camera-video");
  const $cameraCanvas = document.getElementById("camera-canvas");
  
  if (!currentStream || !$cameraVideo.videoWidth || !$cameraVideo.videoHeight) return;

  // Set canvas size to video size
  $cameraCanvas.width = $cameraVideo.videoWidth;
  $cameraCanvas.height = $cameraVideo.videoHeight;
  
  // Draw frame to canvas
  const ctx = $cameraCanvas.getContext("2d");
  ctx.drawImage($cameraVideo, 0, 0, $cameraCanvas.width, $cameraCanvas.height);
  
  // Convert canvas to blob/file (WebP, regardless of device)
  $cameraCanvas.toBlob(blob => {
    if (!blob) return;
    const file = new File([blob], "captured_photo.webp", { type: "image/webp" });

    selectedImageFile = file;
    $imageName.textContent = file.name;
    $imagePill.hidden = false;

    updateSendButton();
    closeCamera();
  }, "image/webp", 0.9);
}
// --------------------

function updateTempSiteRequired() {
  const hasTempValue = $vitalTemp.value.trim() !== "";
  $siteReq.hidden = !hasTempValue;
}

function hasAnyContent() {
  return !!(
    $vitalTemp.value.trim() ||
    $vitalPulse.value.trim() ||
    $vitalResp.value.trim() ||
    $bpSys.value.trim() ||
    $bpDia.value.trim() ||
    $notes.value.trim() ||
    selectedImageFile
  );
}

function updateSendButton() {
  const hasPatient = $patientId.value.trim() !== "";
  const hasContent = hasAnyContent();
  $btnSend.disabled = !(hasPatient && hasContent);
}

function handleImageSelect(e) {
  const file = e.target.files[0];
  if (!file) return;

  e.target.value = ""; // reset
  reencodeToWebp(file, (webpFile) => {
    if (!webpFile) {
      selectedImageFile = file;
      $imageName.textContent = file.name;
      $imagePill.hidden = false;
      updateSendButton();
      return;
    }
    selectedImageFile = webpFile;
    $imageName.textContent = webpFile.name;
    $imagePill.hidden = false;
    updateSendButton();
  });
}

// Re-encode any image File to WebP via an offscreen canvas so the output
// is always WebP regardless of the device's capture/upload format (JPEG/PNG/etc).
function reencodeToWebp(file, cb) {
  const url = URL.createObjectURL(file);
  const img = new Image();
  img.onload = () => {
    URL.revokeObjectURL(url);
    const canvas = document.createElement("canvas");
    canvas.width = img.naturalWidth || img.width;
    canvas.height = img.naturalHeight || img.height;
    try {
      const ctx = canvas.getContext("2d");
      ctx.drawImage(img, 0, 0, canvas.width, canvas.height);
      canvas.toBlob(blob => {
        if (!blob) { cb(null); return; }
        const webpName = file.name.replace(/\.[^.]+$/, "") + ".webp";
        cb(new File([blob], webpName, { type: "image/webp" }));
      }, "image/webp", 0.9);
    } catch (err) {
      console.error("WebP re-encode failed", err);
      cb(null);
    }
  };
  img.onerror = () => {
    URL.revokeObjectURL(url);
    console.error("Could not load image for re-encode");
    cb(null);
  };
  img.src = url;
}

function removeImage() {
  selectedImageFile = null;
  if ($inputPhoto) $inputPhoto.value = "";
  if ($inputUpload) $inputUpload.value = "";
  $imagePill.hidden = true;
  updateSendButton();
}

function getCaptureTypes() {
  const types = [];
  const hasVitals = (
    $vitalTemp.value.trim() ||
    $vitalPulse.value.trim() ||
    $vitalResp.value.trim() ||
    $bpSys.value.trim() ||
    $bpDia.value.trim()
  );

  // Fallback notes into vitals if other vitals exist, else it's just notes
  if (hasVitals || $notes.value.trim()) types.push("vitals");
  if (selectedImageFile) types.push("photo");

  return types;
}

function handleSubmit(e) {
  e.preventDefault();

  const patientId = $patientId.value.trim();
  if (!patientId || !hasAnyContent()) return;

  $btnSend.disabled = true;
  $btnSend.querySelector("span").textContent = "Sending...";
  $btnSend.classList.add("sending");

  const types = getCaptureTypes();

  setTimeout(() => {
    const item = {
      id: Date.now(),
      patientId: patientId,
      types: types,
      status: "pending", // matches mockup
    };
    sentItems.unshift(item);
    updateSentList();
    clearForm();

    $btnSend.querySelector("span").textContent = "Send to hospital";
    $btnSend.classList.remove("sending");
    updateSendButton();

    setTimeout(() => {
      item.status = "sent";
      updateSentList();
    }, 2500);
  }, 500);
}

function clearForm() {
  $patientId.value = "";
  $vitalTemp.value = "";
  $tempSite.value = "";
  $vitalPulse.value = "";
  $vitalResp.value = "";
  $bpSys.value = "";
  $bpDia.value = "";
  $notes.value = "";
  removeImage();
  updateTempSiteRequired();
}

function updateSentList() {
  if (sentItems.length === 0) {
    $sentEmpty.style.display = "block";
    $sentList.innerHTML = "";
    return;
  }

  $sentEmpty.style.display = "none";
  $sentList.innerHTML = "";

  for (const item of sentItems) {
    const li = document.createElement("li");
    li.className = "sent-item";

    const isPending = item.status === "pending";
    const statusClass = isPending ? "status-pending" : "status-sent";
    
    // SVG Icons
    const iconSent = `<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="20 6 9 17 4 12"></polyline></svg>`;
    const iconPending = `<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"></circle><polyline points="12 6 12 12 16 14"></polyline></svg>`;
    
    const icon = isPending ? iconPending : iconSent;
    const text = isPending ? "pending" : "sent";

    li.innerHTML = `
      <span class="sent-info">${escHtml(item.patientId)} · ${escHtml(item.types.join(" + "))}</span>
      <span class="sent-status ${statusClass}">
        ${icon} ${text}
      </span>
    `;

    $sentList.appendChild(li);
  }
}

function escHtml(str) {
  if (str == null) return "";
  return String(str)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

init();
