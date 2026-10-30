--!strict

ui.motion("zero", { kind = "transition", duration = 100 })
ui.motion(0, { kind = "transition", duration = "slow" })
ui.motion(0, { kind = "spring", stiffness = 170 })

local motion = ui.motion(0, { kind = "spring", stiffness = 170, damping = 0.8 })
motion:to("ten")
local position: string = motion:get()
local velocity: string = motion:velocity()

return ui.panel {}
