# Anno editor

A desktop editor for Anno 117: Pax Romana and Anno 1800. It opens the game you have installed, lets you browse its files, edit islands, and edit UI screens. It saves your work as a mod folder for the Anno mod loader. It never changes the shipped game files.

You need a legal install of Anno 117, Anno 1800, or both. The editor reads the install. It does not bundle any game data or artwork.

## Start here

1. Build and run the editor. See "Building from source" below.
2. Pick a game on the first screen.
3. Create or open a project. A project is one mod.
4. Edit an island or a UI screen, or extract a file from the asset browser.
5. Save with Ctrl S. Then use Install to copy the mod into the game.

## Building from source

You need a Rust toolchain and a working graphics driver. On Linux the editor uses Vulkan. Windows code is in the tree and uses DirectX 12, but it has not been tested on a native Windows machine. Linux is the tested platform.

```sh
cargo build --release
cargo run --release
```

The built program is `target/release/anno-editor`. `cargo run --release` opens the game picker.

To open one game directly:

```sh
cargo run --release -- --game 1800
cargo run --release -- --game 117
cargo run --release -- "/path/to/the/game/folder"
```

Replace the example path with your own install folder. You can also set `ANNO1800_DIR` or `ANNO117_DIR` to the install folder for that game. Run with `--help` to see the full usage line. `--open <virtual path>` opens a file from inside the game archives once a game is chosen.

Tests that read game files run only when `ANNO117_DIR` (or `ANNO1800_DIR`) is set; without it they pass without checking anything. `cargo test --bin anno-editor editor::` runs the window tests: the whole editor in GPUI's test platform, driven by keys, with no display or GPU and a config folder of its own under the temp directory.

## Pick a game

The first screen shows one card per game. It uses cover art from your local Steam cache when Steam has it. If there is no cached art, the card still shows the game title. No art is downloaded.

Click a card to open that game. Press 1 for Anno 1800 or 2 for Anno 117. Press Enter to open the last game you picked.

The editor looks for installs on its own. It checks Steam libraries and, on Windows, Ubisoft Connect install records. It reads small local files and registry entries. It does not start Steam or Ubisoft Connect. On Linux it also checks the Flatpak Steam location.

A folder only counts as an install if it has the right game program in `Bin/Win64/` and readable archives in `maindata/`. If the editor cannot find your copy, use Choose folder on the card and pick the install folder yourself. If you have several copies of the same game, the editor asks which one to open. It remembers your choice.

To change games later, use Switch game in the File or project menu. If you have unsaved edits, it asks whether to save or discard them first.

Each game keeps its own projects, layouts, caches, and extracted files. Work for one game does not mix with work for the other.

## The four workspaces

| Workspace | Shortcut | What you do there |
|---|---|---|
| Project | Ctrl 0 | Set up the mod, install it, manage its islands, models, and dependencies |
| Assets | Ctrl 1 | Browse every file in the game archives and preview or extract files |
| Island | Ctrl 2 | Sculpt and paint islands, place props, set harbours, rivers, and resource slots |
| UI (experimental) | Ctrl 3 | Edit the game's compiled UI screens on a canvas |

Other keys that work everywhere:

| Key | Action |
|---|---|
| Ctrl S | Save the project |
| Ctrl Z / Ctrl Shift Z, Ctrl Y | Undo / redo in the Island and UI workspaces |
| Ctrl N | New project |
| Ctrl O | Open a mod folder |
| Ctrl Shift U | Install the mod into the game |
| F5 | Test in game, when a saved test island is ready |
| Ctrl Q | Quit (asks Save / Discard / Cancel when there are unsaved edits; so do File → Quit and the window's close button) |

Panels can be closed and brought back from the View menu. View also has Reset panel layout if you lose a panel. View → Theme switches between Dark, Light and Match system (follows the desktop's light or dark setting); the choice is kept in `theme.json` in the config folder. The 3D view, the game's UI canvas and picture backdrops look the same in every theme.

## Set up a project

A project is one mod. Everything you save goes into that mod, and nothing else.

Use File, New project to make one, or File, Open mod folder to open an existing mod folder. That can be a folder in a git repository or anywhere else on disk. The editor reopens the project you used last. Recent projects are listed in the File menu and the project menu.

In the Project workspace, the Overview tab edits the mod details the loader shows, such as name, id, version, author, description, and category. Anno 117 mods also have a difficulty field. Overview shows whether the copy inside the game is current, older than your project, or missing. It also lists the islands and other edited files in the mod. From there you can open, rename, or delete a project island. Deleting asks for confirmation twice.

The Models tab lists the models your mod carries and the models its dependencies provide, with thumbnails and the islands that use them. Add model brings in a model from disk. Remove drops a model that no island uses.

The Dependencies tab lists mods already installed for this game. Tick one to make your project require it. Its models then show up in your project so islands that use them draw correctly. If you save an island that uses a model from a mod you have not required, the editor warns you and offers to require it.

Changing the mod id renames the project folder when the folder is named after the old id. A folder with any other name stays where it is. If an older copy stays in the game under the old id, the editor tells you.

## Browse assets

Open the Assets workspace with Ctrl 1. It shows every file from the installed archives as one tree, with later archives layered over earlier ones the way the game loads them.

Search to find files by name. Turn on the overrides only filter to see files that exist in more than one archive. The preview pane shows which archives carry the selected file.

Previews cover the common cases. Textures and PNG images show as images. Text files show as text. Other files show a hex view of the start of the file. Model files show a 3D preview.

The archive view is read only. To work on a file, use Extract to write a copy to disk, edit that copy, then use Use in mod on the same file. Or use Replace in mod and pick an edited file directly. Either way it lands in the mod at the same in game path. Islands and UI files also have a direct open action that jumps to the right editor.

## Edit an island

Open the Island workspace with Ctrl 2. The Islands tab lists your project islands first, then every shipped island grouped by province. Use the filter to narrow the list. Click an island to open it.

Opening a shipped island does not add it to your mod. Press Ctrl S while it is open, or use the add to mod action, to copy it into the project. From then on you edit the copy. The original in the archive stays untouched.

### Look around

The Island workspace has a tilted 3D view and a top down view. Both show the same island.

| Input | Action |
|---|---|
| Mouse wheel | Zoom toward the ground under the cursor |
| Right drag in the tilted view | Orbit |
| Right or middle drag | Pan |
| Left click and drag | Use the current tool |
| F | Frame the island or the selection |
| [ and ] | Make the brush smaller or larger |

The scene shows terrain with its ground materials, cliff and prop models, buildings placed as objects, and the sea. Use the scene layers panel to show or hide water, grid overlays, buildable land, ground vegetation preview, and sound and blocker markers.

### Tools

Pick tools from the toolbar or with number keys in toolbar order. Common letter keys: Q select, W move, E rotate, R scale, G grid, S scatter, T tint, H harbour, M resource slot, V river and lake. Delete removes the selection. Ctrl D duplicates it. Esc cancels a drag or drops a river draft and clears the selection.

Numbers in the tool bar, such as radius, strength, and density, work three ways. Drag across one to change it, click it to type a value, or scroll over it to nudge it.

| Tool group | What it does |
|---|---|
| Sculpt: Raise, Lower, Smooth, Flatten, Fill, Scrape, Noise | Change terrain height with a brush. Hold Shift with Raise or Lower to do the opposite. Flatten, Fill, and Scrape use the height where the stroke starts unless you set a target height. |
| Paint | Paint ground materials, such as grass, dirt, rock, and sand, from the material palette. |
| Tint (T) | Paint the tint that colors the ground and tinted models. |
| Select (Q, W, E, R) | Click props and objects to select them. Move, rotate, or scale them with the handles or the inspector fields. |
| Place | Put down the prop or object picked in the Library. Click once per placement. |
| Scatter (S) | Paint many props at once from a set you build in the Library. Density and scale variation are in the tool bar. |
| Grid (G) | Paint gameplay cells by hand, or fill them from the terrain with the auto actions in the tool bar. |
| Harbour (H) | Draw harbour spots where players can found a settlement. |
| Resource slot (M) | Place mine, river, and other resource slots the region supports. |
| River and lake (V) | Draw river and lake curves, then carve them into the terrain. |

Every stroke and every generated change is one undo step.

### The Library

The Library tab at the bottom lists what shipped islands place: prop models, decoration models, and object kinds such as waterfalls. Each entry shows a thumbnail and how often shipped islands use it. Search, click an entry, then click the ground with the Place tool.

Thumbnails render in the background the first time you open a tab and are cached on disk. The first visit can take a short while per model. Later visits read the cache. Delete the `thumbnails/` folder in the config folder to render them again.

With the Scatter tool active, clicking a Library entry adds it to the scatter set or removes it. The set shows as chips in the tool bar. Presets fill the set with common trees, bushes, or rocks for the island region.

### Cliffs

The Scatter tool bar has an auto cliffs action. It covers steep ground above the sea with the region cliff walls and large rocks and paints cliff ground under them. One run is one undo step. Use the density control to pack pieces closer or looser. Turn on Replace earlier to remove pieces from earlier runs before placing new ones. Pieces you placed by hand are left alone.

### Harbours and resource slots

Players can only start a settlement where an island has a harbour spot. With the Harbour tool, drag along the shore to draw one, or click once for a short spot. Drag its points to adjust it. Double click a segment to add a point. Delete removes the picked point. You can flip which side is the sea. The overlay shows the construction area that reaches into the water.

With the resource slot tool, click to place a slot of the picked kind. Random slots let the map generator decide the deposit. Fixed slots always give a mine. Some regions have extra kinds, such as marsh slots on Celtic islands in 117 and clay, oil, Arctic gas, and irrigation slots in 1800. The tool bar only offers kinds the island region supports.

Both tools have an auto in region mode. Drag a rectangle and the editor places the best spots it finds inside it, up to the count you set. Check the results by hand. Auto placement follows simple rules about slope, water depth, and spacing and can still pick a bad spot.

On a project island, selecting a slot lets you assign a test map resource, such as copper or iron for a mine. The game stores resources per island, not per slot, so the test map gives the island that deposit and forces that slot on. In 1800 you can also set a slot to Empty to turn it off on the test map, and set test map fertilities in the inspector.

### Rivers and lakes

With the River and lake tool, click to add points. Drag a point or its width handle to shape the curve. Delete removes the picked point. Press Enter or double click to carve. Press Esc to drop the draft.

River mode makes an open curve. Its water level never rises downstream, so the higher end becomes the source. Lake mode makes a closed curve, flat at its lowest point. Carving digs a bed, paints gravel or pebbles under the water, marks the water cells, and adds flow along a river. It is one undo step.

To change an existing river or lake, double click its water, or select it and use Edit curve in the inspector. Carve again to apply the new shape. Deleting the river object alone leaves its carved bed and cells behind. Undo the carve instead if you want the ground back.

### Gameplay grids

The Grid tool paints the cell layers the game reads, such as buildable land, tree cells, forest, environment, and herds. Each layer has an auto action that fills it from the terrain, the placed trees, or the land. Auto fill gameplay grids runs them in order. Auto actions replace the cells they cover, so paint by hand after running them, not before.

Buildable land follows the slope when an island is created or its terrain is imported. Paint it by hand in the Grid tool when you need a flat pad on a slope or want to block building on flat ground. The Buildable scene layer shows unbuildable land, the edge of buildable land, and harbour pads in any tool.

### Island settings

The inspector has an "In game" section for new islands: role (Regular, Start island, or Both), size class, whether the island joins the random pool, and whether it goes on the test map. A Session section sets the vegetation prop set and the ambient preset, which controls light, sky, and weather in game. The editor uses the ambient preset for its own sun and exposure, so the view is close to the game but not exact. The ground vegetation layer previews the small plants the game scatters at run time from the prop set. It is a preview only. Nothing extra is saved.

Other inspector sections list what the island still lacks, such as a harbour, and place sound spots and build blockers. Auto sound spots puts waterfall and cliff sounds where the terrain suggests them. Auto build blockers covers rocks and decorations on buildable land with blocker cubes so players cannot build through them. Both place normal objects that you can select, move, and delete.

### New islands and imports

File, New island opens a wizard. Pick a name, region, map size, and starting terrain. In 117 the regions are Latium and Albion. In 1800 they are the Old World, New World, Arctic, and Enbesa. Sizes come from shipped playable islands. The size class follows the land you sculpt. In 117 you can override it. On the first save, a new island gets its map images and registration so the random map pool can place it.

File, Import island from Anno 1800 (Import island from Anno 117 in 1800) needs both games installed. Pick a source island and a target province. Heights, tint, rivers, and lakes carry over. Ground materials are mapped by kind and closest color. Each source prop is matched to a similar model in this game and scaled to the same height. When nothing matches well in 117, the 1800 model can be copied into the mod instead. Click a row to change a match or leave a model out. Harbours and slots are placed again from the target province records. Gameplay grids are derived fresh. Review the result before you rely on it.

The Island menu imports and exports single files for the open island:

| Action | Effect |
|---|---|
| Import heightmap | Replace heights from an image. An 8 bit image reads as black at -12 m and white at 40 m. A 16 bit image uses the game height units. |
| Import terrain mesh (OBJ/glTF) | Replace heights from a mesh |
| Import tint map | Replace painted tint from an image |
| Export heightmap | Write a 16 bit PNG in game height units |
| Export tint map | Write a PNG of the painted tint |
| Rebuild land and buildable areas | Replace painted Land and Buildable cells from the terrain |
| Bake tint from terrain | Replace painted tint with tint derived from the terrain |
| Link World Machine export | See the next section |

### Save and test an island

Press Ctrl S to save the project. For an island already in the mod, that writes the updated files. For a shipped island you edited, the first save copies it into the mod. A failed save leaves the last good copy in place.

Then install and test:

1. Press Ctrl Shift U, or use Install in Project, Overview. This copies the mod into the game.
2. For a proper island test, press F5 or use File, Test in game. It installs a small test setup next to your mod.
3. Start a new game. In 117, pick the "Editor islands" map type. In 1800, pick a Large world. The Old World Archipelago option is labelled "Editor islands" while the test mod is active. For other 1800 regions, turn on the matching DLC and travel to that region.
4. Turn the test mod off in the game mod browser to get the shipped maps back.

Only saved project islands marked for the test map can be tested this way. Regular islands fill regular slots and start islands fill start slots. If Test in game is greyed out, the menu says what is missing.

## Round trip an island through World Machine

Use this when you want to shape an island in World Machine and see each build in the editor. World Machine is a separate program. You need your own copy.

1. Open the island in the Island workspace.
2. Open Island, Link World Machine export.
3. Pick an export folder. If you have no World Machine project yet, use New World Machine world in the dialog and pick a template: Plateau and cliffs, Bay coast, Mountain, or Highlands. The editor writes a ready made project into the folder.
4. Open that project in World Machine and Build. Its Height and Splatmap outputs write into the same folder.
5. Back in the editor, use Import now in the dialog. The import is one undo step. Leave re import on and later World Machine builds update the island on their own.
6. Sculpt or paint touch ups in the editor if you need them, then save with Ctrl S.

The dialog lists each export channel next to the ground material it will paint, so check the mapping before the first import. The height channel sets terrain height with a metre range and sea level you can adjust in the dialog. The color channel, when present, becomes tint. Each re export replaces heights and mapped materials again, so hand edits to those parts will be overwritten by the next build.

## Where files go

| Place | What is there |
|---|---|
| `~/.config/anno-editor/` | Anno 117 projects, layouts, caches, thumbnails, extracted files, and the shared `games.json` picker settings |
| `~/.config/anno-editor/1800/` | The same, for Anno 1800 |
| `~/.config/anno-editor/mods/<ModID>/` | New 117 projects by default. 1800 projects go under `1800/mods/` |
| Your chosen folder | A project you opened with Open mod folder stays where it is |
| `<game install>/mods/<ModID>/` | The copy Install writes. The game mod loader picks it up on the next game start |

Inside a project folder:

| Entry | Meaning |
|---|---|
| `modinfo.json` | Mod details for the loader. Fields the editor does not edit are kept as they are |
| `data/` | The mod files at their in game paths |
| `.anno-editor/project.json` | Editor settings for the project, such as its islands and test map choices. Install does not copy this folder |

Save rebuilds `data/` next to the old one and swaps it in, so a failed save keeps the last good `data/`. Other files in the folder, such as a `.git` folder or your own notes, are not touched. Install copies the folder without hidden entries, so `.anno-editor/` and `.git/` stay out of the game.

XML changes the editor generates, such as island registration, are written as mod loader ModOps between editor marker comments. Your own patches in the same XML file survive a save. See the [mod loader modinfo reference](https://jakobharder.github.io/anno-mod-loader/modinfo/) for the `modinfo.json` fields.

A project for one game refuses to install into the other game. If the editor settings file cannot be read, the project refuses to open instead of starting with an empty island list. Fix or remove the broken settings file by hand in that case.

## Limitations

Read these before you ship a mod made here.

- Skinned and bone linked model parts are skipped. Animated or posed models can look incomplete in previews and on islands.
- In 117, ground material detail levels past the closest one are downsampled with a close approximation, not the exact game rule. The close up level the game renders near the camera is exact.
- Rebuilt terrain uses the editor heightfield detail. Fine detail from a shipped island can be smoothed where terrain is regenerated.
- The island view is close to the game, not identical. The sky light is a stand in, deep shadows read darker than in game, there are no clouds, and 1800 islands light with a 117 preset. Ground blending in the preview differs from the game blend.
- The UI canvas draws rotated labels unrotated. Labels have no outlines, shadows, or kerning. Widgets that game code places at run time sit at their stored position.
- Timeline clips play. Tween style animation components do not run. Interact mode only finds hover and press clips that use the usual clip names. New tracks cannot animate colors yet.
- Bind a property only writes converters without settings of their own. Game data fields the install does not bind anywhere are not offered. Fields and events are named from the game's executable (and, for screens kept from Anno 1800, Anno 1800's), and a component whose template does not ship by its JSON (`Tween 72c007a3.Started`); a few have no name anywhere and show their id.
- Game view stacks screens at 1920x1080 only. Some bindings do not evaluate, some game object fields stay at their defaults, and a few HUD bars stay empty because their templates are not shipped with the game.
- Deleting a UI widget can leave data binding entries that still name it. Check the Bindings pane after a delete.
- Deleting a river or lake object leaves its bed and cells. Use undo on the carve to remove both.
