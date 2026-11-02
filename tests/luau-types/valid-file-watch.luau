--!strict
local value = services.shared_state { key = 'data', initial = '' }
services.watch_file {
    key = 'data', path = './data.txt',
    on_change = function(event)
        if event.status == 'ready' then
            value:set(event.text)
        elseif event.status == 'failed' then
            value:set(event.error)
        else
            local missing: 'missing' = event.status
            value:set(missing)
        end
    end,
}
return ui.panel { children = { ui.text { text = function() return value:get() end } } }
