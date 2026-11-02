--!strict

return ui.panel {
    children = {
        ui.stack { style = { composite = 1 } },
        ui.button { style = { composite = function() return 'yes' end } },
    },
}
