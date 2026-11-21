import { NativeModules } from 'react-native';

// The raster engine, as the JavaScript side sees it.
//
// The engine is reached through `ErlRaster`, the module `ErlRasterModule.kt` registers. Everything
// below is a thin translation: it renames the platform's promises, keeps one runtime open for the
// life of the application, and turns a missing library into a plain "not available" answer rather
// than an exception, because the engine's worst case is meant to be "as good as it was without me".
//
// Two facts shape the whole surface.
//
// **Pixels never cross this boundary.** The functions here exchange file paths and integers. An
// integer names a buffer living in native memory that JavaScript will never see, and a path names a
// file on the device. Nothing in this file marshals an image through the bridge, because doing so
// would cost more than the work it would be paying for.
//
// **Nothing here is hot.** These are the calls a list makes when it wants a thumbnail written, not
// the calls a frame makes. A caller that finds the engine unavailable simply keeps the path it had.
const ErlRaster = NativeModules?.ErlRaster || null;

/** Copies pixels rather than blending them. Cheap, and visibly blocky when enlarged. */
export const ERL_RASTER_FILTER_NEAREST = 0;

/** Blends neighbouring pixels. What a thumbnail wants. */
export const ERL_RASTER_FILTER_BILINEAR = 1;

// The thermal codes, numbered the way the ABI documents them.
export const ERL_RASTER_THERMAL_NOMINAL = 0;
export const ERL_RASTER_THERMAL_FAIR = 1;
export const ERL_RASTER_THERMAL_SERIOUS = 2;
export const ERL_RASTER_THERMAL_CRITICAL = 3;
export const ERL_RASTER_THERMAL_UNKNOWN = 4;

/** The identifier of the CPU reference, which every build links. */
export const ERL_RASTER_PIPELINE_REFERENCE = 'cpu-scalar';

/** Whether the module is part of this build at all. Says nothing about whether it loaded. */
export function isErlRasterLinked() {
  return !!ErlRaster;
}

/** Whether a runtime is currently open, and the opening in flight if there is one. */
let opened = false;
let opening = null;

/**
 * Whether the library loaded and the ABI it reports.
 *
 * This is the one call that always resolves, and it is the one every caller should make first. A
 * `false` here is not an error: it is the answer that says the device keeps whatever path it had.
 */
export async function isErlRasterAvailableAsync() {
  if (!ErlRaster) {
    return { available: false, reason: 'the erl-raster module is not part of this build' };
  }
  try {
    const answer = await ErlRaster.isAvailable();
    return {
      available: !!answer?.available,
      abiVersion: answer?.abiVersion ?? null,
      reason: answer?.reason ?? null,
    };
  } catch (error) {
    return { available: false, reason: String(error?.message || error) };
  }
}

/**
 * Opens the runtime, or rebuilds it if one is already open.
 *
 * `profileThermal` is the thermal state the profile was measured in, not the state the phone is in
 * now. Passing the wrong one is not an error the engine can detect: it will simply refuse to move
 * between pipelines and report itself untrusted, which is the honest reading of a profile that
 * describes a device that no longer exists.
 */
export async function openErlRasterAsync({ profile = null, profileThermal = ERL_RASTER_THERMAL_UNKNOWN } = {}) {
  if (!ErlRaster) return null;
  const answer = await ErlRaster.open(profile, profileThermal);
  opened = true;
  return answer;
}

/** Releases the runtime and every buffer it was asked to hold. */
export async function closeErlRasterAsync() {
  opened = false;
  if (!ErlRaster) return null;
  try {
    return await ErlRaster.close();
  } catch (_) {
    return null;
  }
}

/** The pipelines this build linked, the CPU reference first. */
export async function erlRasterPipelineNamesAsync() {
  if (!ErlRaster) return [];
  const answer = await ErlRaster.pipelineNames();
  return Array.isArray(answer?.pipelines) ? answer.pipelines : [];
}

/** What the engine is doing: which pipeline ran last, how long it took, and whether it is trusted. */
export async function erlRasterStatusAsync() {
  if (!ErlRaster) return null;
  return ErlRaster.status();
}

/**
 * Opens the runtime once and answers the same promise to every later caller.
 *
 * A list asks for its first thumbnail before it knows whether it will ask for a second, and opening
 * the runtime per request would enumerate the GPU adapters again each time. The promise is kept, not
 * just the result, so two requests racing at start-up still produce exactly one runtime.
 */
export function ensureErlRasterOpenAsync(options = {}) {
  if (!ErlRaster) return Promise.resolve(null);
  if (opened) return Promise.resolve(true);
  if (!opening) {
    opening = openErlRasterAsync(options)
      .catch(() => null)
      .finally(() => {
        opening = null;
      });
  }
  return opening;
}

/**
 * Decodes an image into native memory and answers the integer naming the buffer that holds it.
 *
 * `maxDimension` is the longest edge the decode should land at, or zero for the image's own size. It
 * is a decode hint: the decoder subsamples in powers of two, so the result is the smallest reduction
 * that still leaves the longest edge at or above the request. The buffer is the caller's to release.
 */
export async function decodeErlRasterImageAsync(path, maxDimension = 0) {
  if (!ErlRaster) throw new Error('the erl-raster module is not part of this build');
  return ErlRaster.decodeImage(String(path || ''), Math.max(0, Math.round(maxDimension)));
}

/** Releases a buffer. Releasing an identifier that is not held does nothing. */
export async function releaseErlRasterBufferAsync(id) {
  if (!ErlRaster) return null;
  return ErlRaster.release(Math.round(id));
}

/**
 * Scales a buffer into a new one of exactly the requested size, and answers the new identifier.
 *
 * The answer also carries what the engine measured: the nanoseconds it took, which pipeline ran, how
 * many bytes moved, and whether the profile still describes the condition the device is in.
 */
export async function resampleErlRasterBufferAsync({
  sourceId,
  sourceWidth,
  sourceHeight,
  width,
  height,
  filter = ERL_RASTER_FILTER_BILINEAR,
}) {
  if (!ErlRaster) throw new Error('the erl-raster module is not part of this build');
  return ErlRaster.resample(
    Math.round(sourceId),
    Math.round(sourceWidth),
    Math.round(sourceHeight),
    Math.round(width),
    Math.round(height),
    filter,
  );
}

/**
 * Reads an image, scales it, and writes it out as a file, in one call.
 *
 * The longest edge of the result is at most the box that was asked for and the aspect ratio is
 * preserved. The decode lands as close to the target as a power-of-two subsample allows and the
 * engine scales the rest of the way, which is the division of labour the measurements support: the
 * decoder is good at reading less, the engine is good at resampling what was read.
 *
 * This is the shape a list wants. It answers the path it wrote, the size it wrote, and the same
 * measurements a resample reports.
 */
export async function writeErlRasterThumbnailAsync({
  sourcePath,
  targetPath,
  maxWidth,
  maxHeight,
  filter = ERL_RASTER_FILTER_BILINEAR,
  format = 'jpeg',
  quality = 90,
}) {
  if (!ErlRaster) throw new Error('the erl-raster module is not part of this build');
  return ErlRaster.thumbnail(
    String(sourcePath || ''),
    String(targetPath || ''),
    Math.round(maxWidth),
    Math.round(maxHeight),
    filter,
    String(format || 'jpeg'),
    Math.round(quality),
  );
}

/**
 * The directory the module may write into, asked for once.
 *
 * The answer never changes for the life of the process, so it is kept rather than asked again. The
 * promise is what is kept, so a burst of callers at start-up all await the same question.
 */
let scratchDirectory = null;
export function erlRasterScratchDirectoryAsync() {
  if (!ErlRaster) return Promise.resolve(null);
  if (!scratchDirectory) {
    scratchDirectory = ErlRaster.scratchDirectory()
      .then((answer) => answer?.path || null)
      .catch(() => null);
  }
  return scratchDirectory;
}

/** The plain path behind a `file://` uri, which is what the native side wants to be handed. */
function pathOf(uri) {
  const raw = String(uri || '').trim();
  if (raw.startsWith('file://')) return decodeURIComponent(raw.slice('file://'.length));
  return raw;
}

/** A sequence that keeps two thumbnails asked for in the same millisecond from sharing a name. */
let thumbnailSequence = 0;

/**
 * The whole path for a uri: read it, scale it into the box, and write the result where the module may.
 *
 * The caller names the file rather than the destination, because the directory belongs to the native
 * side. The answer is a `uri` an image component can take, alongside the measurements the engine
 * reports, and `null` when the library is absent, the source was not a file, or anything else went
 * wrong, which leaves the caller holding whatever path it already had.
 */
export async function writeErlRasterThumbnailForUriAsync({
  sourceUri,
  extension = '.jpg',
  maxWidth,
  maxHeight,
  filter = ERL_RASTER_FILTER_BILINEAR,
  format = 'jpeg',
  quality = 90,
}) {
  if (!ErlRaster) return null;
  const sourcePath = pathOf(sourceUri);
  if (!sourcePath) return null;
  const directory = await erlRasterScratchDirectoryAsync();
  if (!directory) return null;
  thumbnailSequence += 1;
  const name = `thumb-${Date.now().toString(36)}-${thumbnailSequence.toString(36)}${extension}`;
  try {
    const written = await writeErlRasterThumbnailAsync({
      sourcePath,
      targetPath: `${directory}/${name}`,
      maxWidth,
      maxHeight,
      filter,
      format,
      quality,
    });
    if (!written?.path) return null;
    return { ...written, uri: `file://${written.path}` };
  } catch (_) {
    return null;
  }
}
