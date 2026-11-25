# Anno editor

A desktop editor for Anno 117: Pax Romana, Anno 1800 and Anno 2070. Point it at the game you have installed and you can browse every file in its archives, edit islands, models and map layouts, and for 117 and 1800 rework the UI screens.

Your work lives in a project folder. For 117 and 1800 a project is a mod for the Anno mod loader, and the editor installs it into the game for you. The shipped game files are never touched.

You need a legal copy of at least one of the games.

## Start here

1. Build and run the editor.
2. Pick a game on the first screen.
3. Create a project or open one. A project is one mod.
4. Edit an island, a model or a UI screen.
5. Save with Ctrl S, then press Install to copy the mod into the game.

## Building from source

You need a Rust toolchain and a working graphics driver. The editor draws with Vulkan on Linux, Metal on macOS and DirectX 12 on Windows. On macOS you also need Xcode or its command line tools.

```sh
cargo build --release
cargo run --release
```

The program ends up in `target/release/anno-editor` and starts on the game picker. To skip the picker:

```sh
cargo run --release -- --game 1800
cargo run --release -- --game 117
cargo run --release -- --game 2070
cargo run --release -- "/path/to/the/game/folder"
```

Setting `ANNO1800_DIR`, `ANNO117_DIR` or `ANNO2070_DIR` to an install folder does the same. `--open <virtual path>` opens a file from the game archives straight away, and `--help` lists the rest.

The 3D views need a GPU with binding arrays and non-uniform indexing. Without them the view tells you so instead of drawing.

On macOS, Command takes the place of Ctrl in every shortcut below.

### Tests

`cargo test` runs the suite. Tests that read game files only check something when `ANNO117_DIR` or `ANNO1800_DIR` is set. `cargo test --bin anno-editor editor::` drives the whole editor by keyboard in GPUI's test platform, with no display or GPU needed.

## Picking a game

The first screen has a card for each game. Click one, or press 1 for Anno 1800, 2 for Anno 117, 3 for Anno 2070, or Enter for the game you used last.

The editor finds installs on its own by reading your Steam libraries, Flatpak Steam included, and Ubisoft Connect's install records on Windows. It never starts either launcher. If it misses your copy, use Choose folder on the card and pick the install directory. A folder inside a CrossOver bottle works too.

File › Switch game changes games later. Each game keeps its own projects, caches and extracted files.

## What each game supports

| | Anno 117 | Anno 1800 | Anno 2070 |
|---|---|---|---|
| Textures | view, export to PNG | view, export to PNG | view, export to PNG |
| Models | 3D view, glTF export, full editing | 3D view, glTF export, full editing | 3D view, glTF export, cfg and effect editing |
| Islands | new, edit, import, install | new, edit, import, install | new, edit, import |
| Map layouts | edit | edit | edit |
| Test in game | F5 | F5 | by hand |
| UI screens | edit | edit | no |

Islands import between all three games. 117 and 1800 can also take models from the other two.

### Anno 2070

2070 has no mod loader, so there is nothing to install into. Its projects write under `data/` and `addondata/`, the two folders the game reads islands from. To play one, copy those folders over a copy of your install.

A new 2070 island starts as a copy of a shipped island of the size and kind you pick, Surface or Deep Ocean. 2070 island files carry coast lines, AI points and animal layers that the editor keeps but can't lay out again, so it starts from a real island rather than an empty one. Deep Ocean islands open under water, the way the game shows them.

## The workspaces

| Workspace | Key | What it's for |
|---|---|---|
| Project | Ctrl 0 | Mod details, install, the mod's islands, models and dependencies |
| Assets | Ctrl 1 | Browse, preview and extract game files |
| Island | Ctrl 2 | Sculpt, paint, place props, harbours, rivers and resource slots |
| Map | Ctrl 3 | Lay out island slots on a session map |
| UI | Ctrl 4 | Edit the game's compiled UI screens |
| Model | Ctrl 5 | Edit a model's meshes, props, effects, lights and animations |

These work everywhere:

| Key | Action |
|---|---|
| Ctrl S | Save the project |
| Ctrl Z, Ctrl Shift Z or Ctrl Y | Undo, redo |
| Ctrl N | New project |
| Ctrl O | Open a mod folder |
| Ctrl Shift U | Install the mod |
| F5 | Test in game |
| Ctrl Q | Quit, asking about unsaved edits |

Panels close and come back from the View menu, and View › Reset panel layout puts everything back. View › Theme picks Dark, Light or whatever your desktop uses.

## Projects

File › New project makes one. File › Open mod folder opens an existing mod anywhere on disk, a git checkout included. The editor reopens your last project on start.

The Project workspace has three tabs. Overview edits what the mod loader shows, such as the name, id, version and author, and tells you whether the copy in the game is current. Models lists the models your mod carries and the ones its dependencies bring. Dependencies lists the mods installed for this game. Tick one to require it, and its models draw in your islands. If you save an island that uses a model from a mod you haven't required, the editor offers to add it.

## Browsing assets

Assets shows every file in the game's archives as one tree, with later archives layered over earlier ones the way the game loads them. Search by name, or show only files that more than one archive overrides.

Textures, models and text preview in place. Anything else shows as hex.

The archives are read only. To change a file, Extract it, edit the copy, then Use in mod. Replace in mod does it in one go with a file you already have. Either way the file lands in the mod at its game path. Islands, models, maps and UI screens also have an action that opens them in their own workspace.

## Editing an island

The Islands tab lists your project's islands first, then every shipped island by province. Opening a shipped island doesn't add it to your mod. The first Ctrl S copies it in, and from then on you're editing your copy.

### Moving around

| Input | Action |
|---|---|
| Mouse wheel | Zoom toward the cursor |
| Right drag | Orbit |
| Middle drag | Pan |
| Left drag | Use the current tool |
| F | Frame the island or the selection |
| [ and ] | Shrink or grow the brush |

The view draws the island the way the game does, with its sea, sky, weather and time of day. Time in the inspector moves the sun, and Day and Night jump to noon and midnight. Models animate while Graphics › Animation is on.

On a slow GPU or a very large screen, try the Medium or Low preset in the Graphics pane next to Layers. Each option there says what it changes. `ANNO_EDITOR_RENDER_SCALE` (0.25 to 1) fixes the render scale before the editor starts.

### Tools

Pick a tool from the toolbar, by its number, or by letter: Q select, W move, E rotate, R scale, T tint, S scatter, G grid, H harbour, M resource slot, V river and lake. Delete removes the selection, Ctrl D duplicates it and Esc lets go.

Numbers in the tool bar can be dragged, typed or scrolled.

| Tool | What it does |
|---|---|
| Sculpt | Raise, lower, smooth, flatten, fill, scrape or roughen the ground. Shift reverses raise and lower. |
| Paint | Lay ground materials from the island's set |
| Tint | Paint the tint that colours ground and tinted models |
| Select | Pick props and move, turn or scale them with the handles or in the inspector |
| Place | Put down whatever you picked in the Library |
| Scatter | Paint many props at once from a set you build in the Library |
| Grid | Paint gameplay cells such as buildable land, or fill them from the terrain |
| Harbour | Draw where players can found a settlement |
| Resource slot | Place mines and the other slots the region allows |
| River and lake | Draw a curve and carve it into the ground |

Every stroke is one undo step.

The Library tab lists everything shipped islands place, with thumbnails and how often each one is used. The Ground material tab holds the island's texture set. Add from set brings in textures from any set the game ships, up to 32. The first change copies a shared set into your mod, so other islands keep theirs.

Harbours and resource slots can also be placed automatically. Drag a rectangle or lasso a ridge or river bank and the editor puts the best spots it finds inside, facing the way shipped ones do. Look them over before you ship.

To lay a river, click out its course and press Enter. Carving digs a bed shaped like the game's own rivers and paints sand, pebbles and rock along it. A lake is a closed curve, flat at its lowest point. Double click any existing river or lake to edit its curve. Only the stretch you changed gets reshaped.

The Grid tool's Auto fill gameplay grids rebuilds every gameplay layer from the terrain and props. Run it before you touch cells by hand, since it replaces what it covers.

### New islands and imports

File › New island walks you through a name, region, size and starting terrain. On first save the island gets its map images and registration, so the random map pool can place it.

File › Import island from… brings an island over from another installed game. Heights, tint, rivers and lakes come across as they are. Harbours, slots and gameplay grids are rebuilt for the new province. Each prop is matched to a similar model in this game, or copied over when nothing comes close, and you can change any match before importing. File › Convert island does the same between regions of one game, Latium to Albion for example.

The Island menu also imports and exports heightmaps, tint maps and terrain meshes (OBJ or glTF). An 8 bit heightmap reads black as -12 m and white as 40 m. A 16 bit one is in the game's own units.

### World Machine

To shape an island in World Machine, open Island › Link World Machine export and pick a folder. If you don't have a World Machine project yet, the dialog writes one from a template. Build in World Machine, then Import now in the editor. With re-import on, every later build updates the island by itself. Each build overwrites heights and painted materials, so save hand touch-ups for last.

### Testing in game

1. Save with Ctrl S.
2. Press F5. This installs the mod along with a small test setup.
3. Start a new game. In 117 pick the Editor islands map type. In 1800 pick a Large world, where the Old World Archipelago option now reads Editor islands.
4. Turn the test mod off in the game's mod browser to get the shipped maps back.

Only saved islands marked for the test map show up. Start islands fill start slots and the rest fill regular ones. If F5 is greyed out, the menu says what's missing.

## Editing a map layout

The Map workspace edits one layout at a time. New starts from an empty sea or a copy of any layout. Open lists your project's layouts first, then the shipped maps. Move, add and turn island slots and assign islands to them. Saving a shipped map asks whether to save it as a new layout or, in 117 and 1800, override it in place.

## Editing a UI screen

The UI files tab lists the game's compiled screens. Open one and it lays out on the canvas with the game's textures and text.

Click selects the deepest widget under the pointer, Esc steps out to its parent, and right click lists everything stacked there. Drag to move, snapping to the page and to neighbours. Drag a handle to resize, and nudge with the arrow keys. F frames the selection and Home shows the whole page.

Most screens are templates nested inside templates. Double click a widget inside one to edit that template in place, with the rest of the screen dimmed around it. Edits there change every place the template is used, and saving writes the template next to the screen.

The other panes:

- **Widgets** adds images, labels, groups and any template a shipped screen uses. Drag one onto the canvas or the tree.
- **Inspector** edits position, size, anchors and every other property.
- **Bindings** draws the screen's data bindings as a graph. Drag from one pin to another to bind a property.
- **Timeline** plays and edits the screen's animation clips. Turn on Record and anything you move on the canvas gets keyed.

Game view stacks the HUD around the open screen at 1920×1080. Sample data fills in lists, labels and icons so the screen looks populated. Interact mode lets you hover and press buttons like a player.

## Editing a model

The Model workspace lists your mod's models, recent ones, and every model the game ships, by folder. Ctrl P finds one by name. You can also open a model from its preview in Assets, from the Project's Models tab, or from a placed building in the Island inspector.

The Hierarchy lists the model's meshes, props, decals, particle effects, lights, sequences and cloth. Meshes with a skeleton show their joints, with whatever hangs on each one. Drag entries to reorder them or onto a joint to attach them.

The view uses the same controls as the island view, with the gizmo on W, E and R. Day and Night (N) switch the lighting, and Sky picks between a plain studio and one of the game's island skies.

The Timeline plays the model's sequences with their animations, effects and sounds. Drag clips to retime them. To key a bone, pick its joint while an animation plays and move it with the gizmo.

Add prop, Add mesh, Add decal, Add particle effect and the light buttons in the Inspector each copy a real entry from a shipped model, so new entries carry every field the game expects. New particle effect builds one from scratch that stays editable.

Ctrl S writes the model into your mod at its game path, along with any effects and animations you changed. Edited effects and animations apply wherever the game uses them. Make a copy for this model keeps your change to this model alone.

## Where files go

| Place | What's there |
|---|---|
| `~/.config/anno-editor/` | Anno 117 projects, caches, thumbnails and extracted files |
| `~/.config/anno-editor/1800/` | The same for Anno 1800 |
| `~/.config/anno-editor/2070/` | The same for Anno 2070 |
| `~/.config/anno-editor/mods/<ModID>/` | New 117 projects. 1800 projects go in `1800/mods/`. |
| `<game install>/mods/<ModID>/` | The copy Install writes |

A project folder holds `modinfo.json`, the mod's files under `data/` at their game paths, and the editor's own settings in `.anno-editor/`. Saving builds a fresh `data/` next to the old one and swaps it in, so a failed save never leaves you with half a mod. Anything else in the folder, like a `.git` directory or your notes, is left alone, and Install skips hidden entries.

Where the editor patches the game's XML, such as registering an island, it writes ModOps between marker comments. Your own patches in the same file survive a save. The [mod loader docs](https://jakobharder.github.io/anno-mod-loader/modinfo/) explain the `modinfo.json` fields.

## Limitations

- The island view is close to the game, not identical. Check lighting, water and effects in game before you ship.
- Asset previews show models still, in their bind pose. The Model workspace plays everything.
- Damage states aren't shown, and a model is lit only by its own lights, not by the props it places.
- Converting models between games keeps static meshes, decals and effects. Animated, skinned and cloth parts stay behind.
- Rebuilding terrain can smooth out fine detail from a shipped island.
- The UI canvas draws rotated labels unrotated and without outlines, shadows or kerning. Tween components don't run.
- Deleting a UI widget can leave bindings that still name it, so check the Bindings pane afterwards.
- Deleting a river or lake leaves its carved bed behind. Undo the carve to remove both.
