# SDL2 demo (`c:` imports)

```sh
sudo apt install libsdl2-dev pkg-config     # or your system's SDL2 development package
pico run examples/sdl2/main.pi
```

`import * as sdl from "c:SDL2/SDL.h"` generates bindings from SDL2's header (functions,
`@pod` structs such as `SDL_Rect` and `SDL_Surface`, enum and `#define` constants);
`project.json` takes SDL2's include and link flags from pkg-config. The program opens a blue
window with a white square for five seconds, or until it's closed.

Not checked automatically (it needs a display): checked by hand on Debian 13 arm64 (PLAN
P9.2).
