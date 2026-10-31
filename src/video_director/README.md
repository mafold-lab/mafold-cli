# Director stage — blocking a scene before it is generated

A blocking file says who is where, doing what, when, and where the camera is. `render.mjs` turns it into a
grey-box animatic (the "白模预演") that goes to Seedance as `reference_video`, so the model follows the
staging, the timing of the action and the camera route instead of inventing them. It is free and takes
about 10–20 s, so iterate on it until the motion is right.

```
node <this folder>/render.mjs scene.json        # → scene.animatic.mp4, .contact.png, .plan.png, .stage.html, …
```

`example.json` is a worked scene: a girl runs along a rooftop, jumps a 2.2 m gap to a roof one storey lower,
lands in a crouch, stands and turns to the camera while a small robot flies to her shoulder; the camera
follows from behind, rises to the side over the gap and ends low in front of her. Start from a copy of it.

## Format

Metres, y up. `box`/`cylinder` `pos` = centre of the bottom face; `plane`/`zone` `pos` = centre.
`size`: box `[w, h, d]`, cylinder `[diameter, h]`, plane `[w, h]`, zone `[w, d]`.
Angles in degrees about +y: an actor's `face` 0 = facing +x, 90 = −z, −90 = +z; a plane's `yaw` 0 faces +z.

- top level: `duration` (s), `fps` (24), `aspect` (`"16:9"`, `"9:16"` …) — make `duration` the scene's
  length and `aspect` the film's ratio.
- `env`: `sky {top, horizon}`, `fog {color, near, far}`, `lights[]` (`hemi {sky, ground, intensity}`,
  `dir {dir, color, intensity, shadow}`). Only enough to read the shapes — it is not the look of the film.
- `set[]`: `{type: box|cylinder|plane|zone, pos, size, color, label, yaw?, glow?}`. Block the set with what
  the action touches (the floor, the edge, the gap, the thing they hide behind) plus a few masses for depth.
  `zone` is drawn only in the plan and the free view (a gap, a mark), never in the shot.
- `actors[]`: `{id, label, kind, height | size, color, keys[]}`
  - `kind: "mannequin"` — a person: head, neck, chest, pelvis, upper and lower arms, hands, thighs, shins,
    feet, with a ball at every joint. `color` is the whole body (give each person their own); the left side
    is drawn a shade darker so crossing limbs stay readable; `jointColor` overrides the joints' grey.
    `height` in metres (1.6 by default).
  - `kind: "orb"` — a small flying thing (a drone, a sprite, a robot ball): `size` (diameter), `color`,
    `color2` (its visor band). Its path is smoothed.
  - a key: `{t, pos, face, pose, blend?, ease?, apex?, note?}`. `pose` = `run | jump | crouch | stand | idle`;
    it starts at its key and blends in over `blend` s (0.2). `ease` (`linear | in | out | inOut`) shapes the
    move that starts at this key. A `jump` key flies a parabola to the next key, its top `apex` m above the
    take-off. The run cycle follows the distance actually covered, so feet don't skate at any speed. A turn
    is a change of `face` between two keys; the head leads it. `note` is written on the plan.
- `camera.keys[]`: `{t, pos, look, fov}` (`fov` = vertical degrees), smoothed in time. One move per shot;
  keep the subject's box inside the frame — the render prints, per actor, the frames where it leaves.

## What the render gives you

Next to the scene (or `--out <dir>`), named after it: `.animatic.mp4` (exactly `duration × fps` frames, no
audio), `.contact.png` (six timestamped frames — look at it first), `.plan.png` (floor plan with paths,
camera route and notes), `.first.png` / `.last.png`, `.framing.json` (per frame, every actor's box in the
shot camera) and `.stage.html` — the stage with this scene inside: director view, a free orbit view with
the camera path and frustum, the plan, a timeline (space plays, ←/→ step, 1/2/3 switch views). Attached to
a message it opens in Mafold as a page the user can scrub and turn.

Needs node 18+, ffmpeg and ffprobe on PATH, and a Chrome, Edge or Chromium (`CHROME=<path>` if it isn't
found). The first run installs playwright-core and fetches three.js into `~/.mafold/cache/director`
(`MAFOLD_DIRECTOR_CACHE` to move it); after that it renders offline, and the same file renders the same
pixels.

## Handing it to the video model

Pass the animatic as `--ref reference_video=scene.animatic.mp4` together with the scene's bible images, and
say in the prompt what to take from it and what not to — then still describe the action and the camera in
words:

```
@视频1 是粗白模预演，只用它的人物走位、动作和时机、镜头位置和运镜路线；不要采用它的白模外观、颜色和场景。
@视频1 里粉色的人形对应少女，白色球体对应小机器人。@图片1（少女）只取长相、发型和服装……
```
