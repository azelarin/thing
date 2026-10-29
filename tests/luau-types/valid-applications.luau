--!strict

local query = ui.state('')
local terminal = { 'foot', '-e' }

local function matches(app: ThingDesktopApplication, text: string): boolean
    if app.no_display or #app.command == 0 then
        return false
    end
    local needle = string.lower(text)
    if string.find(string.lower(app.name), needle, 1, true) then
        return true
    end
    for _, keyword in app.keywords do
        if string.find(string.lower(keyword), needle, 1, true) then
            return true
        end
    end
    return false
end

local function launch(command: { string }, terminal_app: boolean, directory: string?)
    local argv = {}
    if terminal_app then
        table.move(terminal, 1, #terminal, 1, argv)
    end
    table.move(command, 1, #command, #argv + 1, argv)
    services.launch(argv, directory)
end

return ui.panel {
    children = function()
        local rows = {}
        for _, app in services.applications.entries:get() do
            if matches(app, query:get()) then
                table.insert(rows, ui.button {
                    key = app.id,
                    on_click = function()
                        launch(app.command, app.terminal, app.working_directory)
                    end,
                    children = { ui.image { icon = app.icon }, ui.text { text = app.name } },
                })
            end
        end
        return rows
    end,
}
