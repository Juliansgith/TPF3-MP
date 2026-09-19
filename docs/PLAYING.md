# Playing

How to play Transport Fever 3 together with TPF3-MP. The network side is
ready. Installing the part that runs inside the game waits for the game's
release: this page says so where it applies.

## What you need

- Transport Fever 3, the same build and mods as everyone in your room. The
  room compares everyone's content before a game starts.
- The TPF3-MP package for your system, from the project's releases:
  Windows x64, Linux x64 or macOS on Apple silicon. Players on different
  systems can share one room.
- The address of a TPF3-MP server, such as `tpf3mp.example.org:29470`.

## Installing

1. Unpack the package anywhere.
2. **Into the game: on release.** How the package's game library is put
   next to Transport Fever 3 depends on the released game, and this step is
   described here once it is known.

## The launcher

Start the launcher from the package: `Launch TPF3-MP.cmd` on Windows,
`tpf3mp-launcher.sh` on Linux, `tpf3mp-launcher.command` on macOS. It opens
a page in your browser. Keep its window open while you play: closing it
ends your session.

The page works only on your own machine, and only in the tab the launcher
opened: other pages and other programs cannot use it.

1. **Server.** Enter the server's address and the name others will see,
   then **Connect**.
2. **Rooms.** Either create a room, with an optional password, or paste an
   invite someone sent you and **Join room**.
3. **Invite.** In your room, **Copy invite** and send it to your friends,
   for example on Discord. Anyone with the invite (and the password, if you
   set one) can join; keep it within your group.
4. **Ready.** Everyone presses **Ready**. The room's owner then presses
   **Start game**. Everyone's game starts from the owner's world.

The **Game** part of the page follows your game: downloading the room's
world, loading it, and playing. **Chat** reaches everyone in the room.
**Notices** tell you what happened, such as your world being replaced by
the room's, or your connection coming back.

## While you play

- **Joining later.** You can join a game that is already running: the
  room sends you its world, and your game loads it and catches up.
- **Losing the connection.** If your connection or the server drops, the
  launcher rejoins the room by itself, and your game only pauses. If you
  were away too long to catch up, the room sends you its world again.
- **Leaving.** **Leave room** gives up your seat. The owner can also remove
  a player whose game froze; a removed player cannot come back to that
  room.
- **Your world disagrees.** Every few seconds everyone's game compares the
  world with the room's. If yours has drifted, the room sends you its world
  and your game reloads it. A notice says so.
- **Saving.** The room saves everyone's game together from time to time,
  which you notice as a short pause, like an autosave.

## Your identity

The launcher creates a key for you on first use, in your user data folder
(`TPF3-MP/identity.key`). It is what makes you the same player next time,
so you can come back to your seat. There are no accounts or passwords. Keep
the file private, and copy it if you move to another computer.

Other players never see your IP address: everything goes through the
server.

## When something does not work

- **"the server is out of reach over UDP (...) and through wss://..."**:
  your network blocks both routes, or the server is down. Some school,
  office and hotel networks block the UDP the game uses; the launcher then
  tries a WebSocket connection on port 443 by itself. If the server's
  operator gave you a tunnel address, start the launcher with
  `--tunnel <address>`; if your network never passes UDP, add
  `--tunnel-only`.
- **"connected via tunnel"** next to the connection: your network blocks
  UDP, and the game plays through the tunnel. It works, but lost packets
  cost a little more delay.
- **A version mismatch**: your package and the server are different
  versions. The message says which side is older.
- **"players have different game versions or mods"**: someone's game build
  or mod list differs. Everyone needs the same.
- **"too many players are connected from this network"**: the server
  limits connections per network. Close another game, or ask the operator.
- **"the invite or password is not valid"**: the invite is from another
  server, the room closed, or the password is wrong.
