; Training Dummy — Command set (ORIGINAL, clean-room content)
; (c) 2025 Sekou Doumbouya, MIT.
;
; Defines the directional "hold" commands the engine's built-in locomotion
; needs, plus a single attack button, a quarter-circle-forward+a fireball
; motion, and a dragon-punch+a motion.  The [Statedef -1] bridge turns each
; command into a ChangeState when the character has control.

[Command]
name = "a"
command = a
time = 1

[Command]
name = "holdfwd"
command = /$F
time = 1

[Command]
name = "holdback"
command = /$B
time = 1

[Command]
name = "holdup"
command = /$U
time = 1

[Command]
name = "holddown"
command = /$D
time = 1

; Quarter-circle forward + a (D -> DF -> F -> a).
[Command]
name = "fireball"
command = ~D, DF, F, a
time = 20

; Dragon-punch + a (F -> D -> DF -> a).
[Command]
name = "dp"
command = F, D, DF, a
time = 20

; ---------------------------------------------------------------------------
; Command -> state bridge.
[Statedef -1]

; Special moves take priority over the normal attack; check ctrl + standing/walk.
[State -1, fireball]
type = ChangeState
value = 1000
triggerall = command = "fireball"
trigger1 = ctrl
trigger1 = stateno = 0 || stateno = 20

[State -1, dp]
type = ChangeState
value = 1100
triggerall = command = "dp"
trigger1 = ctrl
trigger1 = stateno = 0 || stateno = 20

[State -1, attack]
type = ChangeState
value = 200
triggerall = command = "a"
trigger1 = ctrl
trigger2 = stateno = 20
