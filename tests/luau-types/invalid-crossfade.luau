--!strict
return ui.panel { children = {
    ui.crossfade { progress = 'half', children = { ui.row {}, ui.row {} } },
    ui.crossfade { progress = function() return false end, children = { ui.row {}, ui.row {} } },
} }
