// The React Native side of ERL Raster, in one entry point.
//
// Importing this module is the whole of the JavaScript integration. Everything it exports is
// documented where it is defined: `erlRasterRuntime` is what a caller asks for work (it decodes a
// file into native memory and answers an integer naming the buffer), and `ErlRasterView` is what a
// caller shows that work with (it draws the buffer from where it already sits, without the picture
// passing through Java or JavaScript).
//
// Nothing here has to be read to be used correctly: mount `ErlRasterPhoto` with a uri the way you
// would mount an `Image`, and the module answers `false` to its availability check on a device whose
// library was not shipped, which leaves the caller's own component in place.

export {
  default,
  ErlRasterPhoto,
  ErlRasterView,
  isRasterisableUri,
} from './src/ErlRasterView';

export * from './src/erlRasterRuntime';
