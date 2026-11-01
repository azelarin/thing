return function(reloaded)
    local state = ui.window_state('scene')
    return ui.window {
        key = 'scene',
        app_id = 'org.thing.render.windows',
        title = function()
            return string.format('%s %dx%d', if reloaded then 'reloaded' else 'window',
                state.width:get(), state.height:get())
        end,
        size = if reloaded then { width = 280, height = 180 } else { width = 360, height = 220 },
        style = { background = if reloaded then '#29436f' else '#164d30' },
        children = {
            ui.stack { style = { width = 32, height = 32,
                background = if reloaded then '#dab23d' else '#e15743',
            } },
        },
    }
end
