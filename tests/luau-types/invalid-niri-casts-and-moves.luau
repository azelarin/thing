--!strict

services.niri.move_window_to_workspace(1, "2")
services.niri.move_window_to_workspace("1", "2", "false")
services.niri.move_window_to_output("1", false)
services.niri.move_workspace_to_output(2, "DP-1")
local casts = services.niri.casts:get()
if casts then
    local stream_id: number = casts[1].stream_id
end
return ui.panel {}
