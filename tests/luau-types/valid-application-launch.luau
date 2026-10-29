--!strict

local terminal = { 'foot', '-e' }

local function open_with(app: ThingDesktopApplication, uris: { string })
    if app.accepts ~= 'none' then
        services.applications.launch(app.id, { uris = uris, terminal = terminal })
    end
end

return ui.panel {
    children = function()
        local rows = {}
        for _, app in services.applications.entries:get() do
            if not app.no_display then
                table.insert(rows, ui.button {
                    key = app.id,
                    on_click = function()
                        services.applications.launch(app.id, { terminal = terminal })
                    end,
                    children = { ui.text { text = app.name } },
                })
                for _, action in app.actions do
                    table.insert(rows, ui.button {
                        key = `{app.id}/{action.key}`,
                        on_click = function()
                            services.applications.launch(app.id, { action = action.key })
                        end,
                        children = { ui.text { text = action.name } },
                    })
                end
                table.insert(rows, ui.button {
                    key = `{app.id}/open`,
                    on_click = function()
                        open_with(app, { '/tmp/notes.txt', 'https://example.org' })
                    end,
                    children = { ui.text { text = 'open' } },
                })
            end
        end
        return rows
    end,
}
