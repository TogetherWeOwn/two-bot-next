# TWO community domain

Language for temporary voice rooms, as defined by the approved voice-room specification.

## Language

**Room**:
A temporary voice channel with tracked ownership, distinct from the creator channel that produces it.
_Avoid_: Creator channel (when referring to the temporary room)

**Owner**:
The member currently responsible for a room, who may differ from its original creator.
_Avoid_: Creator (when referring to the current owner)

**Original creator**:
The member remembered as having created the room, or the recipient of its latest explicit transfer.
_Avoid_: First-ever creator

**Caretaker**:
The longest-present human member who inherits ownership when the current owner leaves an occupied room; the original creator remains remembered.
_Avoid_: New original creator

**Admin**:
A member with Manage Channels permission, who may use owner-only room commands in any room.
_Avoid_: Guild owner (when referring to this permission)
