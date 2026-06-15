; Training Dummy — Command set (ORIGINAL, clean-room content)
; (c) 2025 Sekou Doumbouya, MIT.
;
; Defines the directional "hold" commands the engine's built-in locomotion
; needs, plus a single attack button, and the [Statedef -1] bridge that turns
; the attack command into a ChangeState to the attack state (200).

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

; ---------------------------------------------------------------------------
; Command -> state bridge.
[Statedef -1]

[State -1, attack]
type = ChangeState
value = 200
triggerall = command = "a"
trigger1 = ctrl
trigger2 = stateno = 20
