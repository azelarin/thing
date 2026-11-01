--!strict

return ui.panel {
    children = {
        ui.image { source = "https://art.example/a.png", revision = true },
        ui.image { source = "https://art.example/b.png", revision = function() return {} end },
    },
}
