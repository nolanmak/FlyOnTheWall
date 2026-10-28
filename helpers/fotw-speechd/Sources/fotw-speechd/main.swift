// fotw-speechd: the FluidAudio speech helper for FlyOnTheWall and Murmur
// (#243). See docs/SPEECHD_PROTOCOL.md and helpers/fotw-speechd/README.md.
import SpeechdCore
#if FOTW_FLUIDAUDIO
import SpeechdFluid
#endif

#if FOTW_FLUIDAUDIO
SpeechdMain.run(Array(CommandLine.arguments.dropFirst())) { store in FluidEngines.all(store: store) }
#else
// Built without FluidAudio: only `--fake-engines` serves anything.
SpeechdMain.run(Array(CommandLine.arguments.dropFirst())) { _ in [] }
#endif
