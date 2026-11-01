--!strict

local name: ThingCursor = "hand"

return ui.panel {
    children = {
        ui.button { cursor = "not-allowed" },
        ui.row { cursor = function() return 3 end },
    },
}
