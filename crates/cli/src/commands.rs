/// Type of possible CLI commands: whether is can be executed right there or needs to be sent as request
pub enum CommandType {
    /// Answered by this process alone, whether or not Sonora runs.
    Local(Local),
    /// Sent to the running instance, which is started first if the request allows it.
    Remote(Request),
}

/// Local CLI commands that can be executed without `Sonora` running or requesting is's socket
pub enum Local { 
    Help, 
    Version, 
    // Completions(Shell) 
}

/// CLI commands that need to sent request to the running `Sonora` application
pub enum Request {
    /// Commands serving playback manipulation
    Playback(PlaybackCommand),
    /// Commands serving queue manipulation
    Queue,
    /// Commands serving provider manipulation
    Provider,
    /// Commands serving playback manipulation
    Search
}

/// Commands serving playback manipulation
pub enum PlaybackCommand {
    /// Triggers current playback's Play. Stands for `sonora play`
    Play,
    /// Pauses current playback. Stands for `sonora pause`
    Pause,
    /// Plays next song in the queue. Stands for `sonora next`
    Next,
    /// Plays previous song in the queue. Stands for `sonora prev`
    Previous,
    /// Puts current playback's slider on provided procentage (0% is start, 100% is end of the track). Stands for `sonora seek N`, where N is in [0, 100]
    Seek(i8),
    /// Returns current track's information. Stands for `sonora current`
    CurrentTrack,
    /// Returns or sets volume in order to `Switch`. Stands for `sonora volume < +/-N|N >`
    Volume(Option<Level>),
    /// Returns or sets playback shuffle in order to `Switch`. Stands for `sonora shuffle <on|off|toggle>`
    Shuffle(Option<Switch>),
    /// Returns or sets playback repeat in order to `Repeat`. Stands for `sonora repeat <off|all|one>`
    Repeat(Option<Repeat>)
}

pub enum Queue {
    Add
}

pub enum Level { 
    To(u8), 
    By(i8) 
}

pub enum Switch { 
    On, 
    Off, 
    Toggle 
}

pub enum Repeat {
    Off,
    RepeatAll,
    RepeatOne
}