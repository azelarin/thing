--!strict
local progress = ui.state(0)
return ui.panel { children = {
    ui.crossfade {
        progress = function() return progress:get() end,
        style = { width = 100, height = 40, background = '#181818' },
        children = { ui.text { text = 'before' }, ui.text { text = 'after' } },
    },
    ui.crossfade {
        progress = ui.transition(function() return progress:get() end, { duration = 200 }),
        children = { ui.row {}, ui.row {} },
    },
} }
