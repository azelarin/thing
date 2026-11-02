--!strict

return ui.panel {
    children = {
        ui.image { source = "a.png", blur = "4" },
        ui.image { source = "b.png", saturation = function() return true end },
    },
}
