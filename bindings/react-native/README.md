# erl-raster for React Native

**A picture that is drawn on the screen without a copy of it passing through Java or JavaScript.**

This package is the React Native binding of [ERL Raster](../..). It is the display half of the
library: the decode produces pixels in native memory, and these components put those pixels on the
screen from where they already are. It is the reason the library exists as a *view* and not only as a
function that hands a buffer back.

You do not have to understand any of that to use it. Mount `ErlRasterPhoto` with a uri the way you
would mount an `Image`, and read the four steps below.

---

## 1. What you need

- A React Native application with an Android build, React Native 0.71 or later.
- The `liberl_raster.so` object for each ABI you ship. Either build it (§5) or take the one this
  package was published with.
- Nothing else. There is no C++ to compile, no CMake file to write and no Podfile to touch.

## 2. Install

```sh
npm install erl-raster-react-native
```

## 3. Build the native object, once

The engine is Rust. If you have a Rust toolchain, the package builds it for you:

```sh
npx erl-raster-build-android            # arm64-v8a
npx erl-raster-build-android arm64-v8a armeabi-v7a
```

which is `node_modules/erl-raster-react-native/scripts/build-android.sh` with the same arguments, and
which needs `cargo install cargo-ndk` and an `ANDROID_NDK_HOME`. If you have no Rust, skip this step:
the package ships with the object staged, and a device whose ABI was not shipped simply reports the
engine as unavailable and keeps whatever component you were using before.

## 4. Register the module

One line, in the list of packages your application already builds:

```kotlin
// MainApplication.kt
import com.erl.mobile.ErlRasterPackage

override fun getPackages(): List<ReactPackage> =
  PackageList(this).packages.apply {
    add(ErlRasterPackage())
  }
```

That registers two things: the module, which is how the engine is *asked* for work, and the view
manager, which is how a result reaches the screen. Leave the line out and everything below still
imports and still runs — it just always takes the fallback.

## 5. Use it

```jsx
import { View } from 'react-native';
import { ErlRasterPhoto } from 'erl-raster-react-native';

export function Tile({ uri }) {
  return (
    <ErlRasterPhoto
      uri={uri}
      style={{ width: 160, height: 120, borderRadius: 12 }}
      contentFit="cover"
      maxDimension={480}
    />
  );
}
```

`ErlRasterPhoto` behaves like an image component: give it a uri, it shows the picture; give it a
`fallback`, and it shows that while it cannot. `onLoad` and `onError` are called exactly where any
other image component calls them, so a shimmer, a skeleton or a border keeps working unchanged.

The one number worth setting is `maxDimension`: the longest edge the file should be *decoded* at. It
is what turns a twelve megapixel photograph into a 480-pixel picture before it is ever allocated,
which is where most of the saving is. Zero means the picture's own size, which is only right for
something shown full screen.

## 6. The four things you can import

| Export | What it is |
|---|---|
| `ErlRasterPhoto` | The component above. A uri in, a picture on screen. |
| `ErlRasterView` | The raw surface. A picture *identifier* in, not a uri: use it when you already hold an engine buffer. |
| `isRasterisableUri(uri)` | Whether `ErlRasterPhoto` can draw this uri. Ask before choosing, when you have another component to fall back on. |
| `erlRasterRuntime` | The rest of the engine, for work that is not a view: decode to a buffer, resample, write a thumbnail to a file. |

Everything else in `erlRasterRuntime` is documented at its definition. The short version:

```js
import {
  writeErlRasterThumbnailForUriAsync,   // read a file, scale it, write a thumbnail, answer its uri
  resampleErlRasterBufferAsync,         // scale a buffer the engine already holds
  decodeErlRasterImageAsync,            // decode a file into native memory, answer its identifier
  releaseErlRasterBufferAsync,          // let an identifier go
  isErlRasterAvailableAsync,            // the one call that always resolves; make it first
} from 'erl-raster-react-native';
```

## 7. The two rules that are not obvious

**Never put `backgroundColor` in the style of an `ErlRasterPhoto`.** Not on this component, and not
on an `ErlRasterView`. It is a surface: the window punches a hole in itself to show it, and a
`backgroundColor` on the view fills that hole and hides the picture behind it. The colour you want
behind a picture belongs in the `background` prop, packed as `0xAARRGGBB` — the presenter clears with
it before it draws:

```jsx
<ErlRasterPhoto uri={uri} style={styles.tile} background={0xff1f3d33} />
```

A background set on an *ancestor* or a sibling view is fine: it is behind the hole rather than in it.
Rounded corners (`borderRadius` with `overflow: 'hidden'`) work, and so does anything drawn *on top*
of the picture.

**A uri that is not a file is not a failure.** The engine reads paths, and only paths: a
`content://` uri belongs to another process's grant and an `http` one is bytes that are not here yet.
Both are answered `false` by `isRasterisableUri`, and the honest thing for a caller that has one is to
keep the image component it was already using. This is what a list does:

```jsx
{isRasterisableUri(uri)
  ? <ErlRasterPhoto uri={uri} style={style} contentFit="cover" maxDimension={480} />
  : <Image source={{ uri }} style={style} resizeMode="cover" />}
```

## 8. Why it is fast, in one paragraph

A picture is decoded once, into native memory, at the size it will be shown. It is uploaded to the
device once, and it stays there. A view that is scrolled, resized or asked for a different crop of a
picture it already holds redraws without moving a byte. And because a picture is named by the
identifier its caller gave it, two views showing *the same* picture share one upload: the second one
borrows the texture the first already made. That last one is what a list or a grid produces
constantly, and it is the case the numbers in the main README are about.

## 9. Other ABIs, and the object's name

`erl_raster` is the name of the object, which is why it is `liberl_raster.so` and not
`liberl_raster_react_native.so`. Each ABI gets its own directory:

```
android/src/main/jniLibs/
  arm64-v8a/liberl_raster.so
  armeabi-v7a/liberl_raster.so
  x86_64/liberl_raster.so
```

A device whose ABI is not in there is not a crash: `isErlRasterAvailableAsync()` answers `available:
false` with the reason, and `ErlRasterPhoto` draws its `fallback`.

## 10. iOS

Not yet. The presenter exists and is written against a `CAMetalLayer`, and the ABI in
`erl_raster_view.h` is platform-neutral, but this package ships the Android side only. A build for an
Apple target links the same Rust crate with the `gpu` feature on and reaches it through the same
handful of functions; what is missing is the Swift and Objective-C++ that hands it a layer.

## 11. License

MIT OR Apache-2.0, at your option. See `LICENSE-MIT` and `LICENSE-APACHE` at the repository root.
