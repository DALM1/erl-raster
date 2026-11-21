import { useEffect, useRef, useState } from 'react';
import { requireNativeComponent } from 'react-native';

import {
  decodeErlRasterImageAsync,
  ensureErlRasterOpenAsync,
  isErlRasterLinked,
  releaseErlRasterBufferAsync,
} from './erlRasterRuntime';

// The engine's zero-copy presenter, as a React Native component.
//
// This is the display half of the library. Every other entry point in `erlRasterRuntime.js` produces
// an image *somewhere else* — a file, a buffer — and then hands a name back. This one draws. It is
// handed the identifier a decode or a resample already answered, and the picture goes to the screen
// from the memory it was produced in, without a copy of it passing through Java or JavaScript. That
// is why it takes an integer rather than a uri: a uri would mean reading and decoding the file again
// on the view's side, which is the work the engine was asked to do once.
//
// The component is a surface, so it is not a drop-in replacement for `Image`: it is opaque, and the
// picture has to be decoded first. It exists for the content that is produced by the engine and shown
// immediately.
//
// Properties
//
//   sourceId    Identifier of a picture the engine holds, from `decodeErlRasterImageAsync` or
//               `resampleErlRasterBufferAsync`. `0` or absent draws nothing.
//   filter      `ERL_RASTER_FILTER_NEAREST` or `ERL_RASTER_FILTER_BILINEAR`. Bilinear by default,
//               which is what a scaled-down picture wants.
//   contentFit  `'contain'` shows the whole picture, centred, with the background around it;
//               `'cover'` fills the view and crops the overhang. `'contain'` by default.
//   background  Colour of the area the picture does not cover, packed as `0xAARRGGBB`. Opaque black
//               by default.
//
//   A `backgroundColor` set in `style` hides the picture, and it is not a subtlety worth discovering
//   twice: this view is a surface, the window punches a hole in itself to show it, and that colour
//   fills the hole. The colour behind a picture belongs in the `background` property above, which the
//   presenter clears with before it draws.
//
// A picture the engine no longer holds draws nothing at all, which is what a caller that released it
// while the view was still on screen should see.
//
// The call is native on Android and unimplemented on iOS for now, where the same presenter is waiting
// for a `CAMetalLayer` instead of a surface.
const ErlRasterView = requireNativeComponent('ErlRasterView');

/**
 * The plain path behind a uri, or an empty string when the uri is not a file on this device.
 *
 * The engine reads a path, and only a path: a `content://` uri is another process's grant, an `http`
 * one is bytes that are not here yet, and neither is something a decoder can be pointed at. A caller
 * whose picture is remote keeps its own image component until it has a file, which is the honest
 * division of labour — downloading is not what this library is for.
 */
function localPathOf(uri) {
  const raw = String(uri || '').trim();
  if (raw.startsWith('file://')) {
    const path = decodeURIComponent(raw.slice('file://'.length));
    return path.startsWith('/') ? path : '';
  }
  return raw.startsWith('/') ? raw : '';
}

/**
 * The path the engine would read for this uri, or an empty string when it cannot read it.
 *
 * Two different things can stand in the way and both are answered here: the module is not in this
 * build, and the uri is not a file on this device. Keeping the pair in one place is what lets a
 * caller ask the very question the component asks, so a screen choosing between this component and
 * the one it used before chooses the way the component itself would.
 */
function rasterisablePath(uri) {
  return isErlRasterLinked() ? localPathOf(uri) : '';
}

/**
 * Whether `ErlRasterPhoto` can draw this uri — for a caller that has another component to fall back
 * on.
 *
 * A remote picture is not a failure, it is bytes that are somewhere else, and the caller is the one
 * who knows what to draw until they arrive. Asking first is how a list keeps its own image component
 * for those and hands over only the file-backed ones — mounting this component on a remote uri would
 * draw the stand-in *and* let the stand-in decode a file this component is about to decode again.
 */
export function isRasterisableUri(uri) {
  return !!rasterisablePath(uri);
}

/**
 * Wraps a packed colour into the signed 32-bit integer the native side is given.
 *
 * `0xAARRGGBB` is unsigned, so every opaque colour is past 2^31 and the bridge saturates it to
 * `INT32_MAX` on the way over — which reaches the presenter as mid-grey rather than as the colour
 * anybody asked for. The wrap is JavaScript's own `ToInt32`, and it is what keeps this property's
 * documentation true.
 */
function packedBackground(value) {
  return typeof value === 'number' && Number.isFinite(value) ? value | 0 : undefined;
}

/**
 * One file, one decode size, one picture — held for as long as a view shows it.
 *
 * Why this exists is a measurement rather than a preference. Two tiles showing the same file asked
 * for it twice, decoded it twice, and uploaded it twice: 1 559 280 bytes each, and two copies of one
 * picture in graphics memory, because the identifier a decode answers is a new one every time. The
 * native side shares a texture between views that name the *same* identifier — it logs `borrowed`
 * for the second one and moves no bytes — so the only thing missing was for the second view to name
 * the first one's.
 *
 * The promise is shared rather than the result, because two views mounted together start their
 * effects together: the second has to be able to wait on the first one's decode rather than start a
 * decode of its own. The hold count is what keeps the entry honest — the picture is let go when the
 * last view showing it does, not when the first one stops asking.
 */
const sharedPictures = new Map();

function holdPicture(path, maxDimension) {
  // The two are joined with a character a path cannot contain, so no pair of them can spell another
  // pair's key.
  const key = `${maxDimension}\u0000${path}`;
  let entry = sharedPictures.get(key);
  if (!entry) {
    entry = {
      holders: 0,
      picture: (async () => {
        const open = await ensureErlRasterOpenAsync({});
        if (!open) return null;
        return decodeErlRasterImageAsync(path, maxDimension);
      })().catch(() => null),
    };
    sharedPictures.set(key, entry);
  }
  entry.holders += 1;
  return { key, entry };
}

function letPictureGo(key, entry) {
  entry.holders -= 1;
  if (entry.holders > 0) return;
  sharedPictures.delete(key);
  // A picture nobody is showing is let go whether it has arrived yet or not: a decode that lands
  // after the last view has gone has no owner left to release it.
  entry.picture
    .then((decoded) => {
      const id = decoded?.id || 0;
      if (id) void releaseErlRasterBufferAsync(id).catch(() => {});
    })
    .catch(() => {});
}

/**
 * A picture on the screen, decoded and drawn by the engine.
 *
 * This is the component an application is meant to use. It is handed a uri like any other image
 * component and it answers like one: it shows the picture, or it shows `fallback` while it cannot.
 * What it does in between is the point. The file is decoded straight into native memory, at no more
 * than `maxDimension` on its longest edge, and the identifier that answers is handed to the surface
 * view — so the bytes are read once, at the size they will be shown, and never become a bitmap in the
 * Java heap or a copy on the bridge in between.
 *
 * `maxDimension` is the single most useful number here, and it is a decode-time one: a twelve
 * megapixel photograph shown in a 300-pixel tile is decoded as a 300-pixel picture, because the
 * decoder subsamples in powers of two rather than decoding all of it and throwing most away. Zero
 * means the picture's own size, which is only right for something shown full screen.
 *
 * Two views handed the same file at the same `maxDimension` share one decode and one picture: the
 * second waits on the first one's decode and names the same identifier, so the engine uploads the
 * bytes once and lends the texture to the second view. That is the case a list or a grid produces
 * constantly, and it is worth the map it takes to notice it.
 *
 * `fallback` is rendered whenever the engine cannot serve the picture: the module is not in this
 * build, the uri is not a file, or the decode failed. Exactly one of the two is on screen at a time —
 * the surface is opaque, and drawing a stand-in under it would be a frame of work for a picture
 * nobody can see.
 *
 * `onLoad` reports the picture being shown and `onError` reports a decode that did not produce one —
 * the two every image component answers, so a tile that keeps a shimmer, a skeleton or an overlay
 * over its picture can keep them with this component the way it kept them with the one it replaced.
 * A uri the engine cannot serve is not a load and not an error: nothing was attempted, and the
 * caller is already drawing its own picture for it.
 */
export function ErlRasterPhoto({
  uri,
  style,
  contentFit = 'cover',
  filter,
  background,
  maxDimension = 0,
  fallback = null,
  onLoad,
  onError,
}) {
  const [identifier, setIdentifier] = useState(0);
  const path = rasterisablePath(uri);
  // Read through a ref rather than held in the dependencies below: a caller writing its handlers
  // inline would otherwise change them on every render, and every change would let the picture go
  // and ask for it again.
  const reporting = useRef({});
  reporting.current = { onLoad, onError };

  useEffect(() => {
    if (!path) {
      setIdentifier(0);
      return undefined;
    }
    let cancelled = false;
    const hold = holdPicture(path, maxDimension);
    hold.entry.picture
      .then((decoded) => {
        if (cancelled) return;
        const id = decoded?.id || 0;
        // A decode that answered nothing is a picture the engine could not produce — an unreadable
        // file, a format it does not know — and it is reported rather than swapped for the fallback,
        // because the caller is the one that knows what to put in its place.
        if (!id) {
          reporting.current.onError?.();
          return;
        }
        setIdentifier(id);
        reporting.current.onLoad?.();
      })
      .catch(() => {});
    return () => {
      cancelled = true;
      setIdentifier(0);
      letPictureGo(hold.key, hold.entry);
    };
  }, [path, maxDimension]);

  if (!identifier) return fallback;

  return (
    <ErlRasterView
      style={style}
      sourceId={identifier}
      filter={filter}
      contentFit={contentFit}
      background={packedBackground(background)}
    />
  );
}

export { ErlRasterView };
export default ErlRasterView;
